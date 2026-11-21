//! Multiplexado MPEG-TS: H.264 en Annex B y Opus, sin recomprimir.
//!
//! Adaptado de chiaki-stream (nanpuyue), `src/mux.rs`, AGPL-3.0: la escritura de PAT/PMT/PES con el crate
//! `mpeg2ts` y el corte de Annex B. Cambios: las marcas de tiempo las decide quien llama (`salida.rs`),
//! la salida va a un `Destino` (UDP en producción) y el Opus va con el encabezado de control y los
//! descriptores del estándar de Opus en MPEG-TS (los que escribe ffmpeg y espera MediaMTX).

use std::net::{SocketAddr, UdpSocket};

use mpeg2ts::es::{StreamId, StreamType};
use mpeg2ts::pes::PesHeader;
use mpeg2ts::time::{ClockReference, Timestamp};
use mpeg2ts::ts::payload::{Bytes, Pat, Pes, Pmt};
use mpeg2ts::ts::{
    AdaptationField, ContinuityCounter, Descriptor, EsInfo, Pid, ProgramAssociation, TransportScramblingControl,
    TsHeader, TsPacket, TsPacketWriter, TsPayload, VersionNumber, WriteTsPacket,
};

pub const PID_PMT: u16 = 0x100;
pub const PID_VIDEO: u16 = 0x101;
pub const PID_AUDIO: u16 = 0x102;
pub const ID_VIDEO: u8 = 0xE0;
/// Opus va como "private_stream_1", igual que en ffmpeg.
pub const ID_AUDIO: u8 = 0xBD;

/// Marcas de tiempo a 90 kHz.
pub const HZ: u64 = 90_000;
/// Un cuadro a 60 fps.
pub const CUADRO: u64 = HZ / 60;
/// PAT y PMT se repiten antes de cada keyframe y al menos cada 100 ms.
const CADA_TABLAS: u64 = HZ / 10;
/// Encabezado PES con PTS solo: 9 fijos + 5 del PTS.
const LARGO_ENCABEZADO_PES: usize = 14;
/// Parte opcional del encabezado PES (flags + largo + PTS), la que cuenta en `PES_packet_length`.
const OPCIONAL_PES: usize = 8;
/// MPEG-TS por UDP: 7 paquetes de 188 bytes por datagrama.
pub const DATAGRAMA: usize = 7 * TsPacket::SIZE;

/// A dónde van los paquetes TS (siempre múltiplos de 188 bytes).
pub trait Destino {
    fn enviar(&mut self, datos: &[u8]);
}

impl Destino for Vec<u8> {
    fn enviar(&mut self, datos: &[u8]) {
        self.extend_from_slice(datos);
    }
}

pub struct DestinoUdp {
    sock: UdpSocket,
    dir: SocketAddr,
    errores: u64,
}

impl DestinoUdp {
    pub fn new(dir: SocketAddr) -> std::io::Result<Self> {
        let local: SocketAddr = if dir.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().expect("dirección local");
        Ok(Self { sock: UdpSocket::bind(local)?, dir, errores: 0 })
    }
}

impl Destino for DestinoUdp {
    fn enviar(&mut self, datos: &[u8]) {
        for d in datos.chunks(DATAGRAMA) {
            // Sin socket conectado no llegan los "puerto inalcanzable": si MediaMTX no está, se pierde y listo.
            if let Err(e) = self.sock.send_to(d, self.dir) {
                self.errores += 1;
                if self.errores % 1000 == 1 {
                    crate::aviso!("salida: no pude mandar el MPEG-TS a {} ({e}); van {} errores", self.dir, self.errores);
                }
            }
        }
    }
}

fn pid(n: u16) -> Pid {
    Pid::new(n).expect("pid válido")
}

fn marca(t: u64) -> Timestamp {
    Timestamp::new(t & Timestamp::MAX).expect("33 bits")
}

pub struct Mux<D> {
    destino: D,
    buf: Vec<u8>,
    cc: [ContinuityCounter; 4], // pat, pmt, video, audio
    ultimas_tablas: Option<u64>,
}

impl<D: Destino> Mux<D> {
    pub fn new(destino: D) -> Self {
        Self {
            destino,
            buf: Vec::with_capacity(1 << 18),
            cc: [ContinuityCounter::new(); 4],
            ultimas_tablas: None,
        }
    }

    pub fn destino(&self) -> &D {
        &self.destino
    }

    fn paquete(&mut self, pid: Pid, i: usize, adapt: Option<AdaptationField>, carga: TsPayload) -> mpeg2ts::Result<()> {
        let cc = self.cc[i];
        self.cc[i].increment();
        let p = TsPacket {
            header: TsHeader {
                transport_error_indicator: false,
                transport_priority: false,
                pid,
                transport_scrambling_control: TransportScramblingControl::NotScrambled,
                continuity_counter: cc,
            },
            adaptation_field: adapt,
            payload: Some(carga),
        };
        TsPacketWriter::new(&mut self.buf).write_ts_packet(&p)
    }

    fn tablas(&mut self) -> mpeg2ts::Result<()> {
        let pat = Pat {
            transport_stream_id: 1,
            version_number: VersionNumber::new(),
            table: vec![ProgramAssociation { program_num: 1, program_map_pid: pid(PID_PMT) }],
        };
        self.paquete(pid(0), 0, None, TsPayload::Pat(pat))?;
        let pmt = Pmt {
            program_num: 1,
            pcr_pid: Some(pid(PID_VIDEO)),
            version_number: VersionNumber::new(),
            program_info: vec![],
            es_info: vec![
                EsInfo { stream_type: StreamType::H264, elementary_pid: pid(PID_VIDEO), descriptors: vec![] },
                EsInfo {
                    stream_type: StreamType::Mpeg2PacketizedData,
                    elementary_pid: pid(PID_AUDIO),
                    descriptors: vec![
                        // Registro "Opus" y descriptor de extensión de Opus: 2 canales (estéreo, como la PS5).
                        Descriptor { tag: 0x05, data: b"Opus".to_vec() },
                        Descriptor { tag: 0x7F, data: vec![0x80, 0x02] },
                    ],
                },
            ],
        };
        self.paquete(pid(PID_PMT), 1, None, TsPayload::Pmt(pmt))
    }

    /// Un PES partido en paquetes de 184 bytes; el primero puede llevar adaptación (PCR).
    fn pes(
        &mut self,
        (p, i, id): (u16, usize, u8),
        pts: u64,
        datos: &[u8],
        largo_pes: u16,
        adapt: Option<AdaptationField>,
    ) -> mpeg2ts::Result<()> {
        let largo_adapt = match &adapt {
            None => 0,
            Some(a) => 2 + if a.pcr.is_some() { 6 } else { 0 },
        };
        let entra = (184 - largo_adapt - LARGO_ENCABEZADO_PES).min(datos.len());
        let header = PesHeader {
            stream_id: StreamId::new(id),
            priority: false,
            data_alignment_indicator: true,
            copyright: false,
            original_or_copy: false,
            pts: Some(marca(pts)),
            dts: None,
            escr: None,
        };
        let primero = Pes { header, pes_packet_len: largo_pes, data: Bytes::new(&datos[..entra])? };
        self.paquete(pid(p), i, adapt, TsPayload::PesStart(primero))?;
        for trozo in datos[entra..].chunks(184) {
            self.paquete(pid(p), i, None, TsPayload::PesContinuation(Bytes::new(trozo)?))?;
        }
        Ok(())
    }

    fn mandar(&mut self) {
        self.destino.enviar(&self.buf);
        self.buf.clear();
    }

    /// Escribe un cuadro de video. `clave`: es un keyframe (IDR); se marca como punto de acceso.
    pub fn video(&mut self, au: &[u8], pts: u64, clave: bool) -> mpeg2ts::Result<()> {
        if au.is_empty() {
            return Ok(());
        }
        if clave || self.ultimas_tablas.is_none_or(|t| pts.saturating_sub(t) >= CADA_TABLAS) {
            self.tablas()?;
            self.ultimas_tablas = Some(pts);
        }
        let adapt = AdaptationField {
            discontinuity_indicator: false,
            random_access_indicator: clave,
            es_priority_indicator: false,
            pcr: Some(ClockReference::from(marca(pts))),
            opcr: None,
            splice_countdown: None,
            transport_private_data: vec![],
            extension: None,
        };
        let r = self.pes((PID_VIDEO, 2, ID_VIDEO), pts, au, 0, Some(adapt));
        self.mandar();
        r
    }

    /// Escribe un paquete Opus (sin encabezado; acá se le agrega el de control de MPEG-TS).
    pub fn audio(&mut self, opus: &[u8], pts: u64) -> mpeg2ts::Result<()> {
        if opus.is_empty() {
            return Ok(());
        }
        let mut datos = encabezado_opus(opus.len());
        datos.extend_from_slice(opus);
        let largo = u16::try_from(OPCIONAL_PES + datos.len()).unwrap_or(0);
        let r = self.pes((PID_AUDIO, 3, ID_AUDIO), pts, &datos, largo, None);
        self.mandar();
        r
    }
}

/// Encabezado de control de Opus en MPEG-TS: prefijo 0x3FF sin recortes ni extensión, y el tamaño
/// en bytes de 255 más el resto.
pub fn encabezado_opus(largo: usize) -> Vec<u8> {
    let mut v = vec![0x7F, 0xE0];
    v.extend(std::iter::repeat_n(0xFF, largo / 255));
    v.push((largo % 255) as u8);
    v
}

/// Separa los paquetes Opus de la carga de un PES (cada uno con su encabezado de control).
pub fn partir_opus_ts(mut datos: &[u8]) -> Option<Vec<&[u8]>> {
    let mut out = Vec::new();
    while !datos.is_empty() {
        if datos.len() < 3 || datos[0] != 0x7F || datos[1] & 0xE0 != 0xE0 {
            return None;
        }
        let (recorte_ini, recorte_fin, extension) = (datos[1] & 0x10 != 0, datos[1] & 0x08 != 0, datos[1] & 0x04 != 0);
        let mut i = 2;
        let mut largo = 0usize;
        loop {
            let b = *datos.get(i)?;
            i += 1;
            largo += b as usize;
            if b != 0xFF {
                break;
            }
        }
        i += 2 * (recorte_ini as usize) + 2 * (recorte_fin as usize);
        if extension {
            i += 1 + *datos.get(i)? as usize;
        }
        let paquete = datos.get(i..i + largo)?;
        out.push(paquete);
        datos = &datos[i + largo..];
    }
    Some(out)
}

/// Duración de un paquete Opus en marcas de 90 kHz, según su byte TOC (RFC 6716 §3.1).
pub fn duracion_opus(paquete: &[u8]) -> Option<u64> {
    let toc = *paquete.first()?;
    let config = toc >> 3;
    // Tamaño de cada trama en muestras a 48 kHz.
    let muestras: u64 = match config {
        0..=11 => [480, 960, 1920, 2880][(config % 4) as usize],
        12..=15 => [480, 960][(config % 2) as usize],
        _ => [120, 240, 480, 960][(config % 4) as usize],
    };
    let tramas: u64 = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*paquete.get(1)? & 0x3F) as u64,
    };
    Some(muestras * tramas * HZ / 48_000)
}

/// Corta un cuadro Annex B en unidades NAL (cada una con su código de inicio de 3 o 4 bytes).
/// De chiaki-stream (`split_annexb`).
pub fn partir_annexb(au: &[u8]) -> Vec<&[u8]> {
    let mut inicios = Vec::new();
    let mut i = 0;
    while i + 2 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            inicios.push(if i >= 1 && au[i - 1] == 0 { i - 1 } else { i });
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(inicios.len());
    for (k, &s) in inicios.iter().enumerate() {
        let e = inicios.get(k + 1).copied().unwrap_or(au.len());
        let codigo = if au[s..].starts_with(&[0, 0, 0, 1]) { 4 } else { 3 };
        if e - s > codigo {
            out.push(&au[s..e]);
        }
    }
    out
}

/// Tipo de la unidad NAL H.264 (5 = IDR, 1 = P/B, 7 = SPS, 8 = PPS).
pub fn tipo_nal(nal: &[u8]) -> Option<u8> {
    let codigo = if nal.starts_with(&[0, 0, 0, 1]) {
        4
    } else if nal.starts_with(&[0, 0, 1]) {
        3
    } else {
        return None;
    };
    Some(nal.get(codigo)? & 0x1F)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use mpeg2ts::pes::{PesPacketReader, ReadPesPacket};
    use mpeg2ts::ts::{ReadTsPacket, TsPacketReader};

    /// Lo que sale del mux, leído de vuelta: (id de stream, PTS, carga). El orden se respeta dentro de
    /// cada stream, no entre streams (el lector de `mpeg2ts` entrega cada PES cuando empieza el siguiente).
    pub fn leer_pes(ts: &[u8]) -> Vec<(u8, u64, Vec<u8>)> {
        let mut r = PesPacketReader::new(TsPacketReader::new(ts));
        let mut out = Vec::new();
        while let Some(p) = r.read_pes_packet().expect("TS válido") {
            out.push((p.header.stream_id.as_u8(), p.header.pts.expect("PTS").as_u64(), p.data));
        }
        out
    }

    fn pids(ts: &[u8]) -> Vec<u16> {
        ts.chunks_exact(188).map(|c| (((c[1] & 0x1F) as u16) << 8) | c[2] as u16).collect()
    }

    #[test]
    fn video_y_audio_ida_y_vuelta() {
        let mut m = Mux::new(Vec::new());
        let idr = [0u8, 0, 0, 1, 0x65, 1, 2, 3];
        let grande: Vec<u8> = [0u8, 0, 0, 1, 0x41].iter().copied().chain((0..1000).map(|i| i as u8)).collect();
        m.video(&idr, 90_000, true).unwrap();
        m.audio(&[0xFC, 0xAA, 0xBB], 90_100).unwrap();
        m.video(&grande, 91_500, false).unwrap();
        let ts = m.destino().clone();
        assert_eq!(ts.len() % 188, 0);
        assert!(ts.chunks_exact(188).all(|c| c[0] == 0x47));
        let p = pids(&ts);
        assert!(p.contains(&0) && p.contains(&PID_PMT) && p.contains(&PID_VIDEO) && p.contains(&PID_AUDIO));

        let pes = leer_pes(&ts);
        let de = |id: u8| pes.iter().filter(|p| p.0 == id).cloned().collect::<Vec<_>>();
        assert_eq!(pes.len(), 3);
        assert_eq!(de(ID_VIDEO), vec![(ID_VIDEO, 90_000, idr.to_vec()), (ID_VIDEO, 91_500, grande)]);
        assert_eq!(de(ID_AUDIO), vec![(ID_AUDIO, 90_100, vec![0x7F, 0xE0, 3, 0xFC, 0xAA, 0xBB])]);
    }

    #[test]
    fn contadores_de_continuidad_seguidos_por_pid() {
        let mut m = Mux::new(Vec::new());
        for k in 0..40u64 {
            m.video(&[0, 0, 1, 0x41, k as u8], 90_000 + k * CUADRO, k % 10 == 0).unwrap();
            m.audio(&[0xFC, 1], 90_000 + k * 900).unwrap();
        }
        let ts = m.destino();
        let mut ultimo = std::collections::HashMap::new();
        for c in ts.chunks_exact(188) {
            let p = (((c[1] & 0x1F) as u16) << 8) | c[2] as u16;
            let cc = c[3] & 0x0F;
            if let Some(&a) = ultimo.get(&p) {
                assert_eq!(cc, (a + 1) & 0x0F, "salto de CC en {p:#x}");
            }
            ultimo.insert(p, cc);
        }
    }

    #[test]
    fn la_pmt_declara_h264_y_opus_estereo() {
        let mut m = Mux::new(Vec::new());
        m.video(&[0, 0, 0, 1, 0x65, 9], 90_000, true).unwrap();
        let ts = m.destino();
        let mut r = TsPacketReader::new(&ts[..]);
        let mut pmt = None;
        while let Some(p) = r.read_ts_packet().unwrap() {
            if let Some(TsPayload::Pmt(t)) = p.payload {
                pmt = Some(t);
            }
        }
        let pmt = pmt.expect("hay PMT");
        assert_eq!(pmt.es_info[0].stream_type, StreamType::H264);
        let a = &pmt.es_info[1];
        assert_eq!(a.stream_type, StreamType::Mpeg2PacketizedData);
        assert_eq!(a.descriptors[0].tag, 5);
        assert_eq!(a.descriptors[0].data, b"Opus");
        assert_eq!((a.descriptors[1].tag, a.descriptors[1].data.clone()), (0x7F, vec![0x80, 2]));
    }

    #[test]
    fn la_marca_de_tiempo_da_la_vuelta_a_los_33_bits() {
        let mut m = Mux::new(Vec::new());
        m.video(&[0, 0, 0, 1, 0x65, 9], Timestamp::MAX + 1 + 5, true).unwrap();
        assert_eq!(leer_pes(m.destino())[0].1, 5);
    }

    #[test]
    fn encabezado_opus_y_su_lectura() {
        assert_eq!(encabezado_opus(3), vec![0x7F, 0xE0, 3]);
        assert_eq!(encabezado_opus(255), vec![0x7F, 0xE0, 0xFF, 0]);
        assert_eq!(encabezado_opus(300), vec![0x7F, 0xE0, 0xFF, 45]);
        let mut datos = encabezado_opus(2);
        datos.extend([0xF8, 1]);
        datos.extend(encabezado_opus(300));
        datos.extend(vec![7u8; 300]);
        let p = partir_opus_ts(&datos).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], &[0xF8, 1]);
        assert_eq!(p[1].len(), 300);
        assert!(partir_opus_ts(&[1, 2, 3]).is_none());
    }

    #[test]
    fn duracion_de_paquetes_opus() {
        assert_eq!(duracion_opus(&[0xFC]), Some(1800)); // CELT 20 ms, 1 trama (el silencio de ffmpeg)
        assert_eq!(duracion_opus(&[0xF8]), Some(1800)); // CELT 20 ms mono
        assert_eq!(duracion_opus(&[0xF4]), Some(900)); // CELT 10 ms estéreo
        assert_eq!(duracion_opus(&[0xF1]), Some(1800)); // CELT 10 ms, 2 tramas
        assert_eq!(duracion_opus(&[0xF3, 0x03]), Some(2700)); // CELT 10 ms, 3 tramas (código 3)
        assert_eq!(duracion_opus(&[0x08]), Some(1800)); // SILK 20 ms
        assert_eq!(duracion_opus(&[]), None);
    }

    #[test]
    fn partir_annexb_con_codigos_de_3_y_4_bytes() {
        let mut au = vec![0, 0, 0, 1, 0x67, 0x4D, 0x40, 0x32];
        au.extend([0, 0, 1, 0x68, 0xEE]);
        au.extend([0, 0, 1, 0x65, 0x88]);
        let u = partir_annexb(&au);
        assert_eq!(u.iter().map(|n| tipo_nal(n).unwrap()).collect::<Vec<_>>(), vec![7, 8, 5]);
    }
}

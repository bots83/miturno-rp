//! El cartel "Volvemos enseguida": un MPEG-TS pregrabado de 2 s (un keyframe y cuadros P casi vacíos,
//! 1080p60, con los mismos parámetros que la PS5, y Opus en silencio) que se lee una vez al arrancar
//! y se repite cuando no hay sesión. Lo genera `scripts/video/cartel.sh` del repo principal.

use mpeg2ts::pes::{PesPacketReader, ReadPesPacket};
use mpeg2ts::ts::TsPacketReader;

use crate::mux::{duracion_opus, partir_annexb, partir_opus_ts, tipo_nal, CUADRO, ID_AUDIO, ID_VIDEO};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cartel {
    /// Cuadros H.264 (Annex B), el primero es keyframe. Uno cada `CUADRO` marcas.
    pub video: Vec<Vec<u8>>,
    /// Paquetes Opus (sin encabezado de control) y el momento en que empieza cada uno, en marcas desde
    /// el inicio de la vuelta. Todos terminan antes del final de la vuelta (no se pisan entre vueltas).
    pub audio: Vec<(u64, Vec<u8>)>,
}

impl Cartel {
    /// Duración de una vuelta, en marcas de 90 kHz.
    pub fn duracion(&self) -> u64 {
        self.video.len() as u64 * CUADRO
    }

    pub fn desde_ts(ts: &[u8]) -> Result<Cartel, String> {
        let mut r = PesPacketReader::new(TsPacketReader::new(ts));
        let mut video = Vec::new();
        let mut pts_video = Vec::new();
        let mut opus = Vec::new();
        while let Some(p) = r.read_pes_packet().map_err(|e| format!("MPEG-TS inválido: {e}"))? {
            match p.header.stream_id.as_u8() {
                ID_VIDEO => {
                    pts_video.push(p.header.pts.map(|t| t.as_u64()));
                    video.push(p.data);
                }
                ID_AUDIO => {
                    let paquetes = partir_opus_ts(&p.data).ok_or("audio que no es Opus en MPEG-TS")?;
                    opus.extend(paquetes.into_iter().map(<[u8]>::to_vec));
                }
                _ => {}
            }
        }
        let primero = video.first().ok_or("el cartel no tiene video")?;
        if !partir_annexb(primero).iter().any(|n| tipo_nal(n) == Some(5)) {
            return Err("el cartel tiene que empezar con un keyframe".into());
        }
        let pts: Option<Vec<u64>> = pts_video.into_iter().collect();
        let pts = pts.ok_or("cuadro de video sin PTS")?;
        if pts.windows(2).any(|w| w[1].wrapping_sub(w[0]) != CUADRO) {
            return Err("el cartel tiene que ser de 60 cuadros por segundo, sin B-frames".into());
        }
        let cartel = Cartel { audio: Vec::new(), video };
        let duracion = cartel.duracion();
        let mut audio = Vec::new();
        let mut t = 0;
        for p in opus {
            let d = duracion_opus(&p).ok_or("paquete Opus vacío en el cartel")?;
            if t + d > duracion {
                break;
            }
            audio.push((t, p));
            t += d;
        }
        Ok(Cartel { audio, ..cartel })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::Mux;

    #[test]
    fn lee_un_cartel_escrito_por_el_mux() {
        let mut m = Mux::new(Vec::new());
        m.video(&[0, 0, 0, 1, 0x65, 0xCA], 1000, true).unwrap();
        m.video(&[0, 0, 0, 1, 0x41, 0xCA], 1000 + CUADRO, false).unwrap();
        for k in 0..4u64 {
            m.audio(&[0xF4, k as u8], 1000 + k * 900).unwrap(); // 10 ms cada uno
        }
        let c = Cartel::desde_ts(m.destino()).unwrap();
        assert_eq!(c.video, vec![vec![0, 0, 0, 1, 0x65, 0xCA], vec![0, 0, 0, 1, 0x41, 0xCA]]);
        // La vuelta dura 2 cuadros (3000 marcas): el cuarto paquete (2700..3600) se pasaría y se descarta.
        assert_eq!(c.audio, vec![(0, vec![0xF4, 0]), (900, vec![0xF4, 1]), (1800, vec![0xF4, 2])]);
        assert_eq!(c.duracion(), 3000);
    }

    #[test]
    fn rechaza_un_cartel_sin_keyframe_al_principio() {
        let mut m = Mux::new(Vec::new());
        m.video(&[0, 0, 0, 1, 0x41, 0xCA], 1000, false).unwrap();
        assert!(Cartel::desde_ts(m.destino()).unwrap_err().contains("keyframe"));
    }

    /// El cartel instalado en el servidor de video (si está): 120 cuadros, 2 s, Opus.
    #[test]
    fn el_cartel_instalado_se_lee_bien() {
        let Ok(ts) = std::fs::read("/var/lib/miturno-video/cartel.ts") else {
            eprintln!("sin /var/lib/miturno-video/cartel.ts: no se prueba el cartel real");
            return;
        };
        let c = Cartel::desde_ts(&ts).unwrap();
        assert_eq!(c.video.len(), 120);
        assert_eq!(c.duracion(), 2 * 90_000);
        assert_eq!(c.audio.len(), 100);
        let claves = c.video.iter().filter(|au| partir_annexb(au).iter().any(|n| tipo_nal(n) == Some(5))).count();
        assert_eq!(claves, 1);
    }
}

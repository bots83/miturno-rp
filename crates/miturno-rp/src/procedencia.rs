//! Marca SEI v3 del empalme. El firmante de laboratorio jamás emite el origen PS5.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::cartel::Cartel;
use crate::mux::{partir_annexb, tipo_nal, CUADRO};
use crate::salida::{origen_del_reloj, Salida};

const UUID: &[u8; 16] = b"miturno-vigia-v3";
const MASCARA_PTS: u64 = (1 << 33) - 1;
const DURACION_MAXIMA_MS: u64 = 8 * 60 * 60 * 1000;
const LIMITE_CREDENCIAL: u64 = 1024;
const LIMITE_CLIP: u64 = 16 * 1024 * 1024;
const CREDENCIALES_SYSTEMD: &str = "/run/credentials";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reserva {
    v: u8,
    sesion: String,
    desde_ms: u64,
    hasta_ms: u64,
    semilla_b64: String,
}

fn utc_ms() -> Option<u64> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_millis().try_into().ok()
}

fn leer_limitado(ruta: &Path, limite: u64) -> Result<Vec<u8>, String> {
    let archivo = File::open(ruta).map_err(|_| "no se pudo abrir el archivo")?;
    leer_abierto(archivo, limite)
}

fn leer_abierto(archivo: File, limite: u64) -> Result<Vec<u8>, String> {
    let meta = archivo.metadata().map_err(|_| "no se pudo comprobar el archivo")?;
    if !meta.is_file() || meta.len() > limite {
        return Err("archivo inválido o demasiado grande".into());
    }
    let mut bytes = Vec::new();
    archivo.take(limite + 1).read_to_end(&mut bytes).map_err(|_| "no se pudo leer el archivo")?;
    if bytes.len() as u64 > limite {
        return Err("archivo demasiado grande".into());
    }
    Ok(bytes)
}

pub struct Emisor {
    sesion: String,
    desde_ms: u64,
    hasta_ms: u64,
    clave: SigningKey,
    serie: u64,
    captura_anterior: u64,
    expirado: bool,
    inicio_sintetico_ms: Option<u64>,
}

impl Emisor {
    fn de_reserva(reserva: Reserva, inicio_sintetico_ms: Option<u64>) -> Result<Self, String> {
        if reserva.v != 3
            || reserva.sesion.is_empty() || reserva.sesion.len() > 64
            || !reserva.sesion.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || reserva.desde_ms == 0 || reserva.hasta_ms <= reserva.desde_ms
            || reserva.hasta_ms - reserva.desde_ms > DURACION_MAXIMA_MS
        {
            return Err("reserva de procedencia inválida".into());
        }
        let codificada = Zeroizing::new(reserva.semilla_b64);
        let semilla = Zeroizing::new(STANDARD.decode(codificada.as_bytes()).map_err(|_| "semilla Ed25519 inválida")?);
        let semilla: &[u8; 32] = semilla.as_slice().try_into().map_err(|_| "semilla Ed25519 inválida")?;
        Ok(Self {
            sesion: reserva.sesion,
            desde_ms: reserva.desde_ms,
            hasta_ms: reserva.hasta_ms,
            clave: SigningKey::from_bytes(semilla),
            serie: 0,
            captura_anterior: 0,
            expirado: false,
            inicio_sintetico_ms,
        })
    }

    fn cargar(ruta: &Path, sintesis: Option<u64>) -> Result<Option<Self>, String> {
        let archivo = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(ruta).map_err(|_| "no se pudo abrir la credencial")?;
        let meta = archivo.metadata().map_err(|_| "no se pudo comprobar la credencial")?;
        // systemd puede representar la ACL de lectura del usuario del servicio con una máscara 0440.
        // El directorio oficial y sus permisos se comprueban antes de llegar a esta llamada.
        let bits_ajenos = if sintesis.is_none() { 0o037 } else { 0o077 };
        if !meta.is_file() || meta.permissions().mode() & bits_ajenos != 0 {
            return Err("credencial de procedencia sin permisos privados".into());
        }
        let bytes = Zeroizing::new(leer_abierto(archivo, LIMITE_CREDENCIAL)?);
        if sintesis.is_none() && bytes.as_slice() == b"deshabilitada" {
            return Ok(None);
        }
        let reserva = serde_json::from_slice(&bytes).map_err(|_| "credencial de procedencia inválida")?;
        Self::de_reserva(reserva, sintesis).map(Some)
    }

    fn consumir_reserva(&self, estado: &Path) -> Result<(), String> {
        let marca = estado.join(format!("vigia-usada-{}", self.sesion));
        let archivo = OpenOptions::new().write(true).create_new(true).mode(0o600).open(marca)
            .map_err(|_| "no se pudo reservar la serie o la sesión ya se usó")?;
        archivo.sync_all().and_then(|()| File::open(estado)?.sync_all())
            .map_err(|_| "no se pudo persistir el uso de la reserva".into())
    }

    /// Evalúa la credencial activa sin consumirla ni crear una conexión.
    pub fn verificar_credencial_systemd() -> Result<Option<Self>, String> {
        let Some(credenciales) = std::env::var_os("CREDENTIALS_DIRECTORY") else { return Ok(None) };
        let directorio = Path::new(&credenciales);
        if directorio.parent() != Some(Path::new(CREDENCIALES_SYSTEMD)) {
            return Err("directorio de credenciales fuera de systemd".into());
        }
        let meta = directorio.symlink_metadata().map_err(|_| "no se pudo comprobar el directorio de credenciales")?;
        if !meta.is_dir() || meta.uid() != 0 || meta.permissions().mode() & 0o027 != 0 {
            return Err("directorio de credenciales de systemd sin protección".into());
        }
        let ruta = directorio.join("vigia-procedencia.json");
        if matches!(ruta.symlink_metadata(), Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
            return Ok(None);
        }
        let Some(emisor) = Self::cargar(&ruta, None)? else { return Ok(None) };
        let ahora = utc_ms().ok_or("reloj UTC inválido")?;
        if !(emisor.desde_ms..emisor.hasta_ms).contains(&ahora) {
            return Err("la reserva de procedencia no está vigente".into());
        }
        Ok(Some(emisor))
    }

    /// La ausencia de la credencial conserva la salida previa. Un fallo afecta solo la procedencia.
    pub fn desde_systemd() -> Result<Option<Self>, String> {
        let Some(emisor) = Self::verificar_credencial_systemd()? else { return Ok(None) };
        let estado = std::env::var_os("STATE_DIRECTORY").ok_or("falta StateDirectory para reservar la serie")?;
        emisor.consumir_reserva(Path::new(&estado))?;
        Ok(Some(emisor))
    }

    pub(crate) fn marcar(&mut self, au: &[u8], juego: bool, pts: u64, ahora: Duration) -> Option<Vec<u8>> {
        let captura_ms = match self.inicio_sintetico_ms {
            Some(inicio) => inicio.checked_add(ahora.as_millis().try_into().ok()?)?,
            None => utc_ms()?,
        };
        self.marcar_en(au, juego, pts, captura_ms)
    }

    fn marcar_en(&mut self, au: &[u8], juego: bool, pts: u64, captura_ms: u64) -> Option<Vec<u8>> {
        self.expirado |= captura_ms >= self.hasta_ms;
        if self.expirado || captura_ms < self.desde_ms || captura_ms < self.captura_anterior {
            return None;
        }
        let (inicio, huella) = vcl(au)?;
        let serie = self.serie.checked_add(1)?;
        let origen = match (juego, self.inicio_sintetico_ms) {
            (false, _) => 0,
            (true, None) => 1,
            (true, Some(_)) => 2,
        };
        let mut marca = Vec::with_capacity(139 + self.sesion.len());
        marca.extend_from_slice(UUID);
        marca.extend_from_slice(&[3, origen, self.sesion.len() as u8]);
        marca.extend_from_slice(self.sesion.as_bytes());
        marca.extend_from_slice(&serie.to_be_bytes());
        marca.extend_from_slice(&captura_ms.to_be_bytes());
        marca.extend_from_slice(&(pts & MASCARA_PTS).to_be_bytes());
        marca.extend_from_slice(&huella);
        let firma = self.clave.sign(&marca);
        marca.extend_from_slice(&firma.to_bytes());
        let mut datos = Vec::with_capacity(au.len() + marca.len() + 10);
        datos.extend_from_slice(&au[..inicio]);
        datos.extend_from_slice(&[0, 0, 0, 1, 6]);
        // SEI user_data_unregistered (5), una carga menor a 255 bytes y rbsp_trailing_bits.
        let mut ceros = 0;
        for b in [5, marca.len() as u8].into_iter().chain(marca).chain([0x80]) {
            if ceros == 2 && b <= 3 {
                datos.push(3);
                ceros = 0;
            }
            datos.push(b);
            ceros = if b == 0 { ceros + 1 } else { 0 };
        }
        datos.extend_from_slice(&au[inicio..]);
        self.serie = serie;
        self.captura_anterior = captura_ms;
        Some(datos)
    }
}

/// Huella de todos los NAL VCL (tipos 1 a 5). Incluye la longitud BE y EBSP con encabezado,
/// sin analizar los bits de video ni contar ceros de alineación de Annex B.
fn vcl(au: &[u8]) -> Option<(usize, [u8; 32])> {
    let mut primero = None;
    let mut sha = Sha256::new();
    for nal in partir_annexb(au) {
        if !matches!(tipo_nal(nal), Some(1..=5)) {
            continue;
        }
        primero.get_or_insert_with(|| nal.as_ptr() as usize - au.as_ptr() as usize);
        let prefijo = if nal.starts_with(&[0, 0, 0, 1]) { 4 } else { 3 };
        let bytes = &nal[prefijo..];
        let fin = bytes.iter().rposition(|&b| b != 0)? + 1;
        let largo: u32 = fin.try_into().ok()?;
        sha.update(largo.to_be_bytes());
        sha.update(&bytes[..fin]);
    }
    Some((primero?, sha.finalize().into()))
}

fn instante(marca: u64) -> Duration {
    Duration::from_nanos((marca * 100_000).div_ceil(9))
}

fn emitir_cartel(salida: &mut Salida<Vec<u8>>, hasta: Duration) {
    while salida.proximo() < hasta {
        salida.tick(salida.proximo());
    }
}

fn emitir_juego(salida: &mut Salida<Vec<u8>>, juego: &Cartel, desde: Duration) {
    let mut audio = juego.audio.iter().peekable();
    for (i, cuadro) in juego.video.iter().enumerate() {
        let marca = i as u64 * CUADRO;
        if i > 0 {
            while audio.peek().is_some_and(|(ts, _)| *ts < marca) {
                let (ts, opus) = audio.next().expect("paquete presente");
                salida.audio_ps5(opus, desde + instante(*ts));
            }
        }
        salida.video_ps5(cuadro, 0, desde + instante(marca));
    }
    for (ts, opus) in audio {
        salida.audio_ps5(opus, desde + instante(*ts));
    }
}

/// Doble offline. Usa el mismo empalme y mux sin abrir conexiones ni leer el perfil de la PS5.
pub fn producir_sintetico(argumentos: &[String], destino: &mut impl Write) -> Result<(), String> {
    let [cartel, juego, credencial, inicio_ms] = argumentos else {
        return Err("uso: miturno-rp --procedencia-sintetica CARTEL.ts JUEGO.ts CREDENCIAL.json INICIO_UNIX_MS".into());
    };
    let inicio_ms: u64 = inicio_ms.parse().map_err(|_| "inicio sintético inválido")?;
    let emisor = Emisor::cargar(Path::new(credencial), Some(inicio_ms))?.ok_or("falta firmante sintético")?;
    let cargar_clip = |r: &str| Cartel::desde_ts(&leer_limitado(Path::new(r), LIMITE_CLIP)?);
    let cartel = cargar_clip(cartel)?;
    let juego = cargar_clip(juego)?;
    let primer_juego = Duration::from_secs(2);
    let tramo_juego = instante(juego.duracion());
    let segundo_juego = primer_juego + tramo_juego + Duration::from_secs(2);
    let final_juego = segundo_juego + tramo_juego;
    let final_clips = final_juego + Duration::from_secs(1);
    let final_ms = inicio_ms.checked_add(final_clips.as_millis() as u64).ok_or("intervalo sintético inválido")?;
    if inicio_ms < emisor.desde_ms || final_ms >= emisor.hasta_ms {
        return Err("los clips quedan fuera de la reserva sintética".into());
    }
    let mut salida = Salida::new(cartel, Vec::new(), origen_del_reloj()).con_procedencia(Some(emisor));
    emitir_cartel(&mut salida, primer_juego);
    emitir_juego(&mut salida, &juego, primer_juego);
    salida.sesion_caida(primer_juego + tramo_juego);
    emitir_cartel(&mut salida, segundo_juego);
    emitir_juego(&mut salida, &juego, segundo_juego);
    salida.sesion_caida(final_juego);
    emitir_cartel(&mut salida, final_clips);
    destino.write_all(salida.destino()).map_err(|_| "no se pudo escribir el MPEG-TS sintético".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::{tests::leer_pes, ID_AUDIO, ID_VIDEO};
    use crate::salida::tests::cartel_de_prueba;
    use ed25519_dalek::Signature;

    const INICIO: u64 = 1_790_000_000_000;

    fn reserva() -> Reserva {
        Reserva {
            v: 3,
            sesion: "laboratorio-rp".into(),
            desde_ms: INICIO,
            hasta_ms: INICIO + 10_000,
            semilla_b64: STANDARD.encode((0..32u8).collect::<Vec<_>>()),
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn carga_sei(nal: &[u8]) -> Vec<u8> {
        assert_eq!(&nal[..5], &[0, 0, 0, 1, 6]);
        let mut rbsp = Vec::new();
        let mut ceros = 0;
        for &b in &nal[5..] {
            if ceros == 2 && b == 3 {
                ceros = 0;
                continue;
            }
            rbsp.push(b);
            ceros = if b == 0 { ceros + 1 } else { 0 };
        }
        assert_eq!(rbsp[0], 5);
        assert_eq!(rbsp[1] as usize, rbsp.len() - 3);
        assert_eq!(rbsp.last(), Some(&0x80));
        rbsp[2..rbsp.len() - 1].to_vec()
    }

    #[test]
    fn el_emisor_coincide_con_vectores_ed25519_independientes_y_fija_cada_vcl() {
        let mut emisor = Emisor::de_reserva(reserva(), Some(INICIO)).unwrap();
        let au = [
            0, 0, 0, 1, 9, 0xf0, 0, 0, 0, 1, 0x67, 0x80, 0, 0, 0, 1, 0x68, 0x80,
            0, 0, 0, 1, 0x65, 0x88, 0x42, 0, 0, 3, 0, 0x68, 0x80,
            0, 0, 0, 1, 0x65, 0x43, 0, 0, 3, 1, 0x80, 0,
        ];
        let firmas = [
            "64d54f785d642699187af4148b2de9f2fbb3a1da6b67891ac0be8c2963987956e41f2f216482ba574cc96babf4c34241e5193f7b1660f7d7bbd91ec75897ba0d",
            "3883286e5132f3830da0ca81d87e3469c617c38484a5d7394fa175b6f007f0fad571fc09357469af19d6a663f7769dfa11ba290997f54ff82a659b94b3fc4905",
        ];
        assert_eq!(hex(emisor.clave.verifying_key().as_bytes()), "03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8");
        for (i, origen) in [0, 2].into_iter().enumerate() {
            let serie = i as u64 + 1;
            let pts = 900_000 + serie * 1_500;
            let modificado = emisor.marcar_en(&au, origen == 2, (1 << 33) + pts, INICIO + serie * 17).unwrap();
            let nals = partir_annexb(&modificado);
            assert_eq!(nals.iter().map(|n| tipo_nal(n).unwrap()).collect::<Vec<_>>(), [9, 7, 8, 6, 5, 5]);
            let marca = carga_sei(nals[3]);
            assert_eq!(&marca[..16], UUID);
            assert_eq!(&marca[16..19], &[3, origen, 14]);
            assert_eq!(&marca[19..33], b"laboratorio-rp");
            assert_eq!(&marca[33..41], &serie.to_be_bytes());
            assert_eq!(&marca[41..49], &(INICIO + serie * 17).to_be_bytes());
            assert_eq!(&marca[49..57], &pts.to_be_bytes());
            assert_eq!(hex(&marca[57..89]), "e1cae661135c8a40f9125d20148b3ce8b8d7f65f0944c0ff8fc15ccaa9fc763a");
            assert_eq!(hex(&marca[89..]), firmas[i]);
            let firma = Signature::from_slice(&marca[89..]).unwrap();
            let publica = emisor.clave.verifying_key();
            assert!(publica.verify_strict(&marca[..89], &firma).is_ok());
            let mut alterada = marca[..89].to_vec();
            alterada[17] = 1;
            assert!(publica.verify_strict(&alterada, &firma).is_err());
            let mut alterada = au.to_vec();
            alterada[36] ^= 4; // Segundo VCL.
            assert_ne!(vcl(&alterada).unwrap().1, vcl(&au).unwrap().1);
            assert_eq!(vcl(&modificado), Some((vcl(&au).unwrap().0 + nals[3].len(), vcl(&au).unwrap().1)));
        }
    }

    #[test]
    fn omite_muestras_sin_vcl_capturas_regresivas_y_reservas_caducadas() {
        let mut emisor = Emisor::de_reserva(reserva(), Some(INICIO)).unwrap();
        let au = [0, 0, 1, 0x61, 0x80];
        assert!(emisor.marcar_en(&[0, 0, 1, 0x68, 0x80], true, 1, INICIO).is_none());
        assert!(emisor.marcar_en(&au, true, 1, INICIO - 1).is_none());
        assert!(emisor.marcar_en(&au, true, 2, INICIO + 15).is_some());
        assert!(emisor.marcar_en(&au, true, 3, INICIO + 14).is_none());
        assert!(emisor.marcar_en(&au, true, 4, INICIO + 10_000).is_none());
        assert!(emisor.marcar_en(&au, true, 5, INICIO + 14).is_none());
        assert_eq!(emisor.serie, 1);
        let invalida = Reserva { sesion: "../otra".into(), ..reserva() };
        assert!(Emisor::de_reserva(invalida, Some(INICIO)).is_err());
    }

    struct Carpeta(std::path::PathBuf);
    impl Drop for Carpeta {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn carpeta() -> Carpeta {
        let carpeta = std::env::temp_dir().join(format!(
            "miturno-vigia-emisor-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&carpeta).unwrap();
        Carpeta(carpeta)
    }

    #[test]
    fn admite_la_acl_de_systemd_y_rechaza_el_acceso_ajeno() {
        use std::fs::{set_permissions, write, Permissions};
        use std::os::unix::fs::symlink;

        let carpeta = carpeta();
        let ruta = carpeta.0.join("credencial.json");
        let base = reserva();
        write(&ruta, serde_json::json!({ "v": 3, "sesion": base.sesion,
            "desde_ms": base.desde_ms, "hasta_ms": base.hasta_ms, "semilla_b64": base.semilla_b64,
        }).to_string()).unwrap();
        set_permissions(&ruta, Permissions::from_mode(0o440)).unwrap();
        assert!(Emisor::cargar(&ruta, None).unwrap().is_some());
        assert!(Emisor::cargar(&ruta, Some(INICIO)).is_err());
        let enlace = carpeta.0.join("enlace");
        symlink(&ruta, &enlace).unwrap();
        assert!(Emisor::cargar(&enlace, None).is_err());
        for modo in [0o460, 0o444, 0o441] {
            set_permissions(&ruta, Permissions::from_mode(modo)).unwrap();
            assert!(Emisor::cargar(&ruta, None).is_err());
        }
        set_permissions(&ruta, Permissions::from_mode(0o600)).unwrap();
        assert!(Emisor::cargar(&ruta, Some(INICIO)).unwrap().is_some());
        write(&ruta, b"deshabilitada").unwrap();
        assert!(Emisor::cargar(&ruta, None).unwrap().is_none());
        assert!(Emisor::cargar(&ruta, Some(INICIO)).is_err());
    }

    #[test]
    fn una_reserva_real_no_puede_reiniciar_su_serie() {
        let carpeta = carpeta();
        let emisor = Emisor::de_reserva(reserva(), None).unwrap();
        emisor.consumir_reserva(&carpeta.0).unwrap();
        let reiniciado = Emisor::de_reserva(reserva(), None).unwrap();
        assert!(reiniciado.consumir_reserva(&carpeta.0).is_err());
    }

    #[test]
    fn los_relevos_comparten_serie_y_no_modifican_pts_audio_o_vcl() {
        let cartel = cartel_de_prueba();
        let juego = Cartel { video: vec![vec![0, 0, 0, 1, 0x65, 0x30]], audio: vec![(0, vec![0xfc, 1])] };
        let ensayar = |marcar: bool| {
            let emisor = marcar.then(|| Emisor::de_reserva(reserva(), Some(INICIO)).unwrap());
            let mut salida = Salida::new(cartel.clone(), Vec::new(), 900_000).con_procedencia(emisor);
            emitir_cartel(&mut salida, Duration::from_millis(50));
            emitir_juego(&mut salida, &juego, Duration::from_millis(50));
            salida.sesion_caida(Duration::from_millis(70));
            emitir_cartel(&mut salida, Duration::from_millis(120));
            leer_pes(salida.destino())
        };
        let sin_marcas = ensayar(false);
        let con_marcas = ensayar(true);
        assert_eq!(sin_marcas.len(), con_marcas.len());
        let sin_audio = sin_marcas.iter().filter(|(id, _, _)| *id == ID_AUDIO);
        let con_audio = con_marcas.iter().filter(|(id, _, _)| *id == ID_AUDIO);
        assert!(sin_audio.eq(con_audio));
        let sin_video: Vec<_> = sin_marcas.iter().filter(|(id, _, _)| *id == ID_VIDEO).collect();
        let con_video: Vec<_> = con_marcas.iter().filter(|(id, _, _)| *id == ID_VIDEO).collect();
        assert_eq!(sin_video.len(), con_video.len());
        let mut serie = 0;
        let mut origenes = Vec::new();
        for ((_, pts, base), (_, nuevo_pts, marcado)) in sin_video.into_iter().zip(con_video) {
            assert_eq!(pts, nuevo_pts);
            let nals = partir_annexb(marcado);
            let nal = nals.iter().find(|n| tipo_nal(n) == Some(6)).unwrap();
            let carga = carga_sei(nal);
            serie += 1;
            assert_eq!(u64::from_be_bytes(carga[33..41].try_into().unwrap()), serie);
            assert_eq!(u64::from_be_bytes(carga[49..57].try_into().unwrap()), *pts);
            let mut reconstruido = Vec::new();
            for original in nals.into_iter().filter(|n| tipo_nal(n) != Some(6)) {
                reconstruido.extend_from_slice(original);
            }
            assert_eq!(*base, reconstruido);
            origenes.push(carga[17]);
        }
        let mut relevos = origenes;
        relevos.dedup();
        assert_eq!(relevos, vec![0, 2, 0]);
    }
}

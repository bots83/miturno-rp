//! Estado del vigilante de imagen (`miturno-vigia`) para `GET /control/salud`: lee el `estado.json` que
//! el clasificador escribe cada segundo y lo resume como `vigia`. Cualquier cosa rara (falta, pesa de
//! más, no es un archivo común, JSON roto, forma distinta) da `None`: la api lo cuenta como sin cuadros.

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Más que esto no es un `estado.json` válido (el clasificador escribe menos de 1 KiB).
const MAX_BYTES: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstadoVigia {
    Juego,
    Pantalla,
    Incierto,
    SinCuadros,
}

/// `VideoSalud.vigia` (los nombres los fija el tipo de la api).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaludVigia {
    pub estado: EstadoVigia,
    pub clase: Option<String>,
    pub seguro: bool,
    pub edad_ms: u64,
}

/// El archivo tal como lo escribe el clasificador. `desde` se acepta pero no se usa.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Archivo {
    v: u8,
    ts: u64,
    estado: EstadoVigia,
    clase: Option<String>,
    seguro: bool,
    #[allow(dead_code)]
    desde: u64,
}

fn clase_valida(c: &str) -> bool {
    (1..=32).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// `O_NONBLOCK` de Linux (igual en todas las arquitecturas): el `open` de un FIFO sin escritor no se
/// cuelga. Sin `libc` como dependencia, los valores se fijan acá y un test los ata al comportamiento.
const O_NONBLOCK: i32 = 0o4000;
/// `O_NOFOLLOW` de Linux: 0o400000 salvo en arm/aarch64, donde vale 0o100000.
#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
const O_NOFOLLOW: i32 = 0o100000;
#[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
const O_NOFOLLOW: i32 = 0o400000;

/// Lee `ruta` y devuelve el veredicto con su edad (`ahora_ms − ts`, saturada en 0). Síncrona: abre con
/// `O_NONBLOCK | O_NOFOLLOW` (un FIFO o un enlace no cuelgan ni se siguen) y recién con el archivo abierto
/// chequea con `fstat` que sea común y de a lo sumo 4 KiB, así que no hay carrera entre el chequeo y la
/// lectura; la lectura tiene tope. Desde el servidor se llama con `spawn_blocking` y un timeout.
pub fn leer_vigia(ruta: &Path, ahora_ms: u64) -> Option<SaludVigia> {
    use std::os::unix::fs::OpenOptionsExt;
    let archivo = std::fs::OpenOptions::new().read(true).custom_flags(O_NONBLOCK | O_NOFOLLOW).open(ruta).ok()?;
    let meta = archivo.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return None;
    }
    let mut buf = Vec::new();
    archivo.take(MAX_BYTES + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > MAX_BYTES {
        return None;
    }
    let a: Archivo = serde_json::from_slice(&buf).ok()?;
    if a.v != 1 || !a.clase.as_deref().is_none_or(clase_valida) {
        return None;
    }
    Some(SaludVigia { estado: a.estado, clase: a.clase, seguro: a.seguro, edad_ms: ahora_ms.saturating_sub(a.ts) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AHORA: u64 = 1_795_300_001_500;

    fn en_archivo(nombre: &str, texto: &[u8]) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("vigia-test-{}-{nombre}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ruta = dir.join("estado.json");
        std::fs::write(&ruta, texto).unwrap();
        (dir, ruta)
    }

    fn leer_texto(nombre: &str, texto: &str) -> Option<SaludVigia> {
        let (dir, ruta) = en_archivo(nombre, texto.as_bytes());
        let r = leer_vigia(&ruta, AHORA);
        std::fs::remove_dir_all(dir).unwrap();
        r
    }

    const VALIDO: &str = r#"{"v":1,"ts":1795300000000,"estado":"pantalla","clase":"store","seguro":true,"desde":1795299997000}"#;

    #[test]
    fn archivo_valido_da_los_campos_y_la_edad() {
        assert_eq!(
            leer_texto("valido", VALIDO),
            Some(SaludVigia { estado: EstadoVigia::Pantalla, clase: Some("store".into()), seguro: true, edad_ms: 1500 })
        );
    }

    #[test]
    fn clase_null_y_todos_los_estados() {
        for (e, esperado) in [
            ("juego", EstadoVigia::Juego),
            ("incierto", EstadoVigia::Incierto),
            ("sin_cuadros", EstadoVigia::SinCuadros),
        ] {
            let t = format!(r#"{{"v":1,"ts":1795300001000,"estado":"{e}","clase":null,"seguro":false,"desde":1}}"#);
            let r = leer_texto(e, &t).unwrap();
            assert_eq!((r.estado, r.clase, r.seguro, r.edad_ms), (esperado, None, false, 500));
        }
    }

    #[test]
    fn ts_en_el_futuro_da_edad_cero() {
        let t = VALIDO.replace("1795300000000", "1795300009000");
        assert_eq!(leer_texto("futuro", &t).unwrap().edad_ms, 0);
    }

    #[test]
    fn archivo_que_falta_da_none() {
        assert_eq!(leer_vigia(Path::new("/no/existe/estado.json"), AHORA), None);
    }

    #[test]
    fn mas_de_4_kib_da_none() {
        let relleno = " ".repeat(5 * 1024);
        assert_eq!(leer_texto("grande", &format!("{VALIDO}{relleno}")), None);
    }

    #[test]
    fn una_carpeta_da_none() {
        let dir = std::env::temp_dir().join(format!("vigia-test-{}-carpeta", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(leer_vigia(&dir, AHORA), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn mkfifo(ruta: &Path) {
        let c = std::ffi::CString::new(ruta.to_str().unwrap()).unwrap();
        extern "C" {
            fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
        }
        assert_eq!(unsafe { mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn un_fifo_da_none_sin_bloquear() {
        let dir = std::env::temp_dir().join(format!("vigia-test-{}-fifo", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ruta = dir.join("estado.json");
        mkfifo(&ruta);
        // En un hilo aparte: si el `open` se colgara, el test falla por timeout en vez de colgarse.
        let (tx, rx) = std::sync::mpsc::channel();
        let r2 = ruta.clone();
        std::thread::spawn(move || {
            let _ = tx.send(leer_vigia(&r2, AHORA));
        });
        let r = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("leer_vigia se colgó con un FIFO");
        assert_eq!(r, None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn las_constantes_de_apertura_hacen_lo_que_dicen() {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("vigia-test-{}-flags", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // O_NONBLOCK: abrir un FIFO sin escritor vuelve en el acto (sin la bandera se bloquea).
        let fifo = dir.join("fifo");
        mkfifo(&fifo);
        let (tx, rx) = std::sync::mpsc::channel();
        let f2 = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(std::fs::OpenOptions::new().read(true).custom_flags(O_NONBLOCK).open(&f2).is_ok());
        });
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(true));
        // O_NOFOLLOW: un enlace simbólico no se abre.
        let real = dir.join("real.json");
        std::fs::write(&real, VALIDO).unwrap();
        let enlace = dir.join("enlace.json");
        std::os::unix::fs::symlink(&real, &enlace).unwrap();
        assert!(std::fs::OpenOptions::new().read(true).custom_flags(O_NOFOLLOW).open(&enlace).is_err());
        assert!(std::fs::OpenOptions::new().read(true).custom_flags(O_NOFOLLOW).open(&real).is_ok());
        assert_eq!(leer_vigia(&enlace, AHORA), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn formas_que_no_cierran_dan_none() {
        let casos = [
            ("roto", VALIDO[..40].to_string()),
            ("vacio", String::new()),
            ("v2", VALIDO.replace(r#""v":1"#, r#""v":2"#)),
            ("estado", VALIDO.replace("pantalla", "dormido")),
            ("clase_mayus", VALIDO.replace("store", "Store!")),
            ("clase_vacia", VALIDO.replace("store", "")),
            ("clase_larga", VALIDO.replace("store", &"a".repeat(33))),
            ("de_mas", VALIDO.replace("}", r#","otro":1}"#)),
            ("sin_desde", VALIDO.replace(r#","desde":1795299997000"#, "")),
            ("sin_seguro", VALIDO.replace(r#""seguro":true,"#, "")),
        ];
        for (nombre, t) in casos {
            assert_eq!(leer_texto(nombre, &t), None, "{nombre}");
        }
    }

    #[test]
    fn serializa_en_camel_case() {
        let v = serde_json::to_value(leer_texto("json", VALIDO)).unwrap();
        assert_eq!(v, serde_json::json!({"estado":"pantalla","clase":"store","seguro":true,"edadMs":1500}));
    }
}

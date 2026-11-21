//! La pausa del ciclo guardada en disco, para que un reinicio de miturno-rp (pánico con
//! `Restart=always`, despliegue o reinicio del servidor) no retome una consola tomada en forma local
//! ni despierte una PS5 que el operador mandó a reposo.
//!
//! El archivo (`pausa` de `rp.toml`, por defecto `/var/lib/miturno-rp/pausa`, el `StateDirectory`
//! de la unidad) tiene una sola línea: `tomada` o `reposo`. Se escribe al entrar en la pausa (en un
//! temporal del mismo directorio, con `fsync` y `rename`: nunca queda a medias) y se borra al salir
//! (`/control/reconectar` o `/control/encender`). Si no existe, no hay pausa. Si está corrupto o no
//! se puede leer, se registra y se arranca como sin pausa (y el corrupto se borra).

use std::fmt;
use std::io::Write;
use std::path::Path;

/// Por qué el ciclo está en pausa (no despierta ni conecta solo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pausa {
    /// La PS5 cerró la sesión porque alguien la usa en forma local: espera `/control/reconectar`.
    Tomada,
    /// `/control/reposo`: espera `/control/encender`.
    Reposo,
}

impl Pausa {
    fn texto(self) -> &'static str {
        match self {
            Pausa::Tomada => "tomada",
            Pausa::Reposo => "reposo",
        }
    }
}

#[derive(Debug)]
pub enum ErrorPausa {
    /// El archivo existe pero no dice `tomada` ni `reposo`.
    Corrupta,
    /// No se pudo leer (permisos, E/S).
    Lectura(std::io::Error),
}

impl PartialEq for ErrorPausa {
    fn eq(&self, otro: &Self) -> bool {
        match (self, otro) {
            (ErrorPausa::Corrupta, ErrorPausa::Corrupta) => true,
            (ErrorPausa::Lectura(a), ErrorPausa::Lectura(b)) => a.kind() == b.kind(),
            _ => false,
        }
    }
}

impl fmt::Display for ErrorPausa {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorPausa::Corrupta => f.write_str("no dice \"tomada\" ni \"reposo\""),
            ErrorPausa::Lectura(e) => write!(f, "no lo pude leer: {e}"),
        }
    }
}

pub fn parsear(texto: &str) -> Option<Pausa> {
    match texto.trim() {
        "tomada" => Some(Pausa::Tomada),
        "reposo" => Some(Pausa::Reposo),
        _ => None,
    }
}

/// `Ok(None)` si el archivo no existe.
pub fn leer(ruta: &Path) -> Result<Option<Pausa>, ErrorPausa> {
    // Es una palabra: más de 64 bytes ya es corrupto (y no se lee un archivo enorme).
    let mut buf = Vec::new();
    match std::fs::File::open(ruta) {
        Ok(f) => {
            use std::io::Read;
            f.take(64).read_to_end(&mut buf).map_err(ErrorPausa::Lectura)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ErrorPausa::Lectura(e)),
    }
    std::str::from_utf8(&buf).ok().and_then(parsear).map(Some).ok_or(ErrorPausa::Corrupta)
}

/// Escribe la pausa de forma atómica: temporal en el mismo directorio, `fsync`, `rename` y `fsync`
/// del directorio.
pub fn guardar(ruta: &Path, pausa: Pausa) -> std::io::Result<()> {
    let dir = ruta.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut nombre = ruta.file_name().unwrap_or_default().to_os_string();
    nombre.push(".tmp");
    let temporal = dir.join(nombre);
    let escribir = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&temporal)?;
        f.write_all(pausa.texto().as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        std::fs::rename(&temporal, ruta)?;
        std::fs::File::open(dir)?.sync_all()
    };
    let r = escribir();
    if r.is_err() {
        let _ = std::fs::remove_file(&temporal);
    }
    r
}

/// Borra la pausa; si no existe, no es error.
pub fn borrar(ruta: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(ruta) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Lo que usa el ciclo al arrancar: la pausa guardada, o `None` (sin archivo, corrupto o ilegible,
/// con el motivo en el registro; el corrupto se borra para que no vuelva a molestar).
pub fn cargar(ruta: &Path) -> Option<Pausa> {
    match leer(ruta) {
        Ok(p) => p,
        Err(ErrorPausa::Corrupta) => {
            crate::aviso!("pausa: {} {}; arranco como sin pausa y lo borro", ruta.display(), ErrorPausa::Corrupta);
            if let Err(e) = borrar(ruta) {
                crate::falla!("pausa: no pude borrar {}: {e}", ruta.display());
            }
            None
        }
        Err(e) => {
            crate::falla!("pausa: {}: {e}; arranco como sin pausa", ruta.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn dir(nombre: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("miturno-rp-pausa-{}-{nombre}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parsea_los_dos_estados_y_nada_mas() {
        assert_eq!(parsear("tomada\n"), Some(Pausa::Tomada));
        assert_eq!(parsear("  reposo  "), Some(Pausa::Reposo));
        assert_eq!(parsear(""), None);
        assert_eq!(parsear("tomadaa"), None);
        assert_eq!(parsear("Tomada"), None);
        assert_eq!(parsear("tomada\nreposo\n"), None);
    }

    #[test]
    fn guardar_y_leer() {
        let d = dir("ida-y-vuelta");
        let r = d.join("pausa");
        assert_eq!(leer(&r), Ok(None), "sin archivo: sin pausa");
        guardar(&r, Pausa::Tomada).unwrap();
        assert_eq!(std::fs::read_to_string(&r).unwrap(), "tomada\n");
        assert_eq!(leer(&r), Ok(Some(Pausa::Tomada)));
        guardar(&r, Pausa::Reposo).unwrap();
        assert_eq!(leer(&r), Ok(Some(Pausa::Reposo)), "reemplaza");
        let archivos: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(archivos, vec!["pausa"], "no queda el temporal");
        borrar(&r).unwrap();
        assert_eq!(leer(&r), Ok(None));
        borrar(&r).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn contenido_corrupto_es_error_que_lo_dice() {
        let d = dir("corrupto");
        let r = d.join("pausa");
        std::fs::write(&r, "xyz").unwrap();
        let e = leer(&r).unwrap_err();
        assert!(matches!(e, ErrorPausa::Corrupta), "{e:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn un_archivo_sin_permiso_de_lectura_es_error() {
        let d = dir("permiso");
        let r = d.join("pausa");
        std::fs::write(&r, "tomada\n").unwrap();
        std::fs::set_permissions(&r, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&r).is_err() {
            assert!(matches!(leer(&r), Err(ErrorPausa::Lectura(_))));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn guardar_en_un_directorio_sin_permiso_falla() {
        let d = dir("solo-lectura");
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o500)).unwrap();
        let r = d.join("pausa");
        if std::fs::write(d.join("prueba"), "x").is_err() {
            assert!(guardar(&r, Pausa::Tomada).is_err());
        }
        assert!(guardar(&d.join("no-existe").join("pausa"), Pausa::Tomada).is_err());
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cargar_borra_la_corrupta_y_deja_la_buena() {
        let d = dir("cargar");
        let r = d.join("pausa");
        assert_eq!(cargar(&r), None);
        std::fs::write(&r, "basura").unwrap();
        assert_eq!(cargar(&r), None);
        assert!(!r.exists(), "la corrupta se borra");
        guardar(&r, Pausa::Reposo).unwrap();
        assert_eq!(cargar(&r), Some(Pausa::Reposo));
        assert!(r.exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}

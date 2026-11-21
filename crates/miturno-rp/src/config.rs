//! Configuración: `rp.toml` (sin secretos), `casa.env` (nombre de la conexión de casa, privado) y
//! `ps5.env` (registración de la consola, secreta). Los errores dicen qué archivo y qué campo fallan,
//! nunca el valor.

use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::Deserialize;

pub const RUTA_CONFIG: &str = "/etc/miturno-video/rp.toml";
pub const RUTA_CASA: &str = "/etc/miturno-video/casa.env";
pub const RUTA_PS5: &str = "/etc/miturno-video/ps5.env";
/// Dónde se guarda la pausa del ciclo (`StateDirectory=miturno-rp` de la unidad).
pub const RUTA_PAUSA: &str = "/var/lib/miturno-rp/pausa";

fn ruta_pausa() -> PathBuf {
    RUTA_PAUSA.into()
}

/// `rp.toml`. Todos los campos son obligatorios salvo `pausa`: la instalación (Task 6) los escribe explícitos.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub bitrate_kbps: u32,
    /// Destino del MPEG-TS, `ip:puerto` (MediaMTX lo lee de ahí).
    pub salida_udp: String,
    /// Dirección del servidor HTTP/WebSocket local (Task 5).
    pub http: String,
    /// Cartel "Volvemos enseguida" pregrabado (MPEG-TS de 2 s).
    pub cartel: PathBuf,
    /// Clave pública con la que se validan las credenciales de turno (Task 5).
    pub clave_publica: PathBuf,
    /// Token de servidor a servidor de los endpoints de control (Task 5).
    pub token_control: PathBuf,
    /// Archivo con la pausa del ciclo (consola tomada o reposo), para que sobreviva a un reinicio
    /// (`crate::pausa`). Por defecto, `RUTA_PAUSA`.
    #[serde(default = "ruta_pausa")]
    pub pausa: PathBuf,
}

/// Todo lo que hace falta para despertar la PS5 y abrir una sesión.
#[derive(Clone, PartialEq, Eq)]
pub struct Perfil {
    /// Nombre DDNS de casa. Privado: no se registra.
    pub host: String,
    /// Registración, como texto (relleno con NUL hasta 16 bytes, como chiaki-ng). Secreta.
    pub regist_key: [u8; 16],
    /// "Morning" de la registración. Secreto.
    pub morning: [u8; 16],
    /// Id de la cuenta de PSN que registró la consola. Secreto.
    pub account_id: [u8; 8],
    pub bitrate_kbps: u32,
}

// Nunca se imprime el contenido: ni por error con {:?}.
impl fmt::Debug for Perfil {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Perfil")
            .field("host", &"<oculto>")
            .field("regist_key", &"<oculto>")
            .field("morning", &"<oculto>")
            .field("account_id", &"<oculto>")
            .field("bitrate_kbps", &self.bitrate_kbps)
            .finish()
    }
}

impl Perfil {
    /// Credencial del paquete de encendido: la registración en texto, leída como hexadecimal (igual que chiaki-ng).
    pub fn credencial_despertar(&self) -> Option<u64> {
        let fin = self.regist_key.iter().position(|&b| b == 0).unwrap_or(16);
        let texto = std::str::from_utf8(&self.regist_key[..fin]).ok()?.trim();
        if texto.is_empty() {
            return None;
        }
        u64::from_str_radix(texto, 16).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorConfig(pub String);

impl fmt::Display for ErrorConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ErrorConfig {}

pub(crate) fn leer(ruta: &Path) -> Result<String, ErrorConfig> {
    std::fs::read_to_string(ruta).map_err(|e| {
        let motivo = match e.kind() {
            std::io::ErrorKind::NotFound => "no existe".to_string(),
            std::io::ErrorKind::PermissionDenied => "sin permiso de lectura".to_string(),
            _ => e.to_string(),
        };
        ErrorConfig(format!("no pude leer {}: {motivo}", ruta.display()))
    })
}

pub fn cargar_config(ruta: &Path) -> Result<Config, ErrorConfig> {
    parsear_config(&leer(ruta)?, ruta)
}

pub fn parsear_config(texto: &str, ruta: &Path) -> Result<Config, ErrorConfig> {
    // rp.toml no tiene secretos: el mensaje de toml (que cita la línea) se puede mostrar.
    let c: Config = toml::from_str(texto).map_err(|e| ErrorConfig(format!("{} inválido: {e}", ruta.display())))?;
    if c.salida_udp.parse::<std::net::SocketAddr>().is_err() {
        return Err(ErrorConfig(format!("{}: salida_udp tiene que ser ip:puerto", ruta.display())));
    }
    if c.http.parse::<std::net::SocketAddr>().is_err() {
        return Err(ErrorConfig(format!("{}: http tiene que ser ip:puerto", ruta.display())));
    }
    if c.bitrate_kbps == 0 {
        return Err(ErrorConfig(format!("{}: bitrate_kbps tiene que ser mayor que 0", ruta.display())));
    }
    Ok(c)
}

/// Lee un archivo `CLAVE=valor` (líneas vacías y `#` se ignoran; el valor puede ir entre comillas).
fn parsear_env(texto: &str) -> Vec<(String, String)> {
    texto
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let l = l.strip_prefix("export ").unwrap_or(l);
            let (k, v) = l.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

fn campo<'a>(pares: &'a [(String, String)], clave: &str, ruta: &Path) -> Result<&'a str, ErrorConfig> {
    match pares.iter().rev().find(|(k, _)| k == clave) {
        Some((_, v)) if !v.is_empty() => Ok(v),
        _ => Err(ErrorConfig(format!("falta {clave} en {}", ruta.display()))),
    }
}

fn hex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Arma el perfil desde `casa.env` y `ps5.env`.
pub fn parsear_perfil(
    casa: &str,
    ruta_casa: &Path,
    ps5: &str,
    ruta_ps5: &Path,
    bitrate_kbps: u32,
) -> Result<Perfil, ErrorConfig> {
    let casa = parsear_env(casa);
    let host = campo(&casa, "PS5_HOST", ruta_casa)?.to_string();
    if host.contains(char::is_whitespace) || host.contains('\0') {
        return Err(ErrorConfig(format!("PS5_HOST inválido en {}", ruta_casa.display())));
    }

    let ps5 = parsear_env(ps5);
    let rk = campo(&ps5, "PS5_REGIST_KEY", ruta_ps5)?;
    if rk.len() > 16 || rk.contains('\0') {
        return Err(ErrorConfig(format!(
            "PS5_REGIST_KEY inválida en {}: tiene que ser texto de hasta 16 caracteres",
            ruta_ps5.display()
        )));
    }
    let mut regist_key = [0u8; 16];
    regist_key[..rk.len()].copy_from_slice(rk.as_bytes());

    let morning = hex16(campo(&ps5, "PS5_MORNING", ruta_ps5)?).ok_or_else(|| {
        ErrorConfig(format!("PS5_MORNING inválido en {}: tienen que ser 32 caracteres hexadecimales", ruta_ps5.display()))
    })?;

    let error_cuenta = || {
        ErrorConfig(format!(
            "PS5_ACCOUNT_ID inválido en {}: tiene que ser base64 de 8 bytes",
            ruta_ps5.display()
        ))
    };
    let cuenta = campo(&ps5, "PS5_ACCOUNT_ID", ruta_ps5)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cuenta)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(cuenta.trim_end_matches('=')))
        .map_err(|_| error_cuenta())?;
    let account_id: [u8; 8] = bytes.try_into().map_err(|_| error_cuenta())?;

    let perfil = Perfil { host, regist_key, morning, account_id, bitrate_kbps };
    if perfil.credencial_despertar().is_none() {
        return Err(ErrorConfig(format!(
            "PS5_REGIST_KEY inválida en {}: tiene que ser hexadecimal (como la exporta chiaki-ng)",
            ruta_ps5.display()
        )));
    }
    Ok(perfil)
}

pub fn cargar_perfil(ruta_casa: &Path, ruta_ps5: &Path, bitrate_kbps: u32) -> Result<Perfil, ErrorConfig> {
    parsear_perfil(&leer(ruta_casa)?, ruta_casa, &leer(ruta_ps5)?, ruta_ps5, bitrate_kbps)
}

/// Rutas de los tres archivos; `--config`, `--casa` y `--ps5` las cambian (para pruebas).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rutas {
    pub config: PathBuf,
    pub casa: PathBuf,
    pub ps5: PathBuf,
}

impl Default for Rutas {
    fn default() -> Self {
        Self { config: RUTA_CONFIG.into(), casa: RUTA_CASA.into(), ps5: RUTA_PS5.into() }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Argumentos {
    Correr(Rutas),
    Version,
}

pub fn parsear_argumentos<I: IntoIterator<Item = String>>(args: I) -> Result<Argumentos, ErrorConfig> {
    let mut rutas = Rutas::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let destino = match a.as_str() {
            "--version" | "-V" => return Ok(Argumentos::Version),
            "--config" => &mut rutas.config,
            "--casa" => &mut rutas.casa,
            "--ps5" => &mut rutas.ps5,
            otro => {
                return Err(ErrorConfig(format!(
                    "argumento desconocido: {otro} (uso: miturno-rp [--config rp.toml] [--casa casa.env] [--ps5 ps5.env])"
                )))
            }
        };
        *destino = it.next().ok_or_else(|| ErrorConfig(format!("falta la ruta después de {a}")))?.into();
    }
    Ok(Argumentos::Correr(rutas))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOML: &str = r#"
bitrate_kbps = 8000
salida_udp = "127.0.0.1:9000"
http = "127.0.0.1:7070"
cartel = "/var/lib/miturno-video/cartel.ts"
clave_publica = "/etc/miturno-video/ct-public.pem"
token_control = "/etc/miturno-video/control.token"
pausa = "/var/lib/miturno-rp/pausa"
"#;

    fn r(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn lee_todos_los_campos_de_rp_toml() {
        let c = parsear_config(TOML, &r("rp.toml")).unwrap();
        assert_eq!(
            c,
            Config {
                bitrate_kbps: 8000,
                salida_udp: "127.0.0.1:9000".into(),
                http: "127.0.0.1:7070".into(),
                cartel: r("/var/lib/miturno-video/cartel.ts"),
                clave_publica: r("/etc/miturno-video/ct-public.pem"),
                token_control: r("/etc/miturno-video/control.token"),
                pausa: r("/var/lib/miturno-rp/pausa"),
            }
        );
    }

    #[test]
    fn sin_pausa_en_rp_toml_usa_la_ruta_por_defecto() {
        let sin = TOML.replace("pausa = \"/var/lib/miturno-rp/pausa\"\n", "");
        assert_ne!(sin, TOML);
        assert_eq!(parsear_config(&sin, &r("rp.toml")).unwrap().pausa, r(RUTA_PAUSA));
        assert_eq!(RUTA_PAUSA, "/var/lib/miturno-rp/pausa");
        let otra = TOML.replace("/var/lib/miturno-rp/pausa", "/srv/pausa");
        assert_eq!(parsear_config(&otra, &r("rp.toml")).unwrap().pausa, r("/srv/pausa"));
    }

    #[test]
    fn la_plantilla_del_repo_es_valida() {
        let t = include_str!("../../../config/rp.toml");
        assert_eq!(parsear_config(t, &r("config/rp.toml")).unwrap(), parsear_config(TOML, &r("rp.toml")).unwrap());
    }

    #[test]
    fn rp_toml_con_campo_faltante_o_desconocido_falla() {
        let sin_http = TOML.replace("http = \"127.0.0.1:7070\"\n", "");
        assert!(parsear_config(&sin_http, &r("rp.toml")).unwrap_err().0.contains("http"));
        let extra = format!("{TOML}otro = 1\n");
        assert!(parsear_config(&extra, &r("rp.toml")).is_err());
        let mala = TOML.replace("127.0.0.1:9000", "localhost");
        assert!(parsear_config(&mala, &r("rp.toml")).unwrap_err().0.contains("salida_udp"));
        let http_malo = TOML.replace("127.0.0.1:7070", "localhost:7070");
        assert!(parsear_config(&http_malo, &r("rp.toml")).unwrap_err().0.contains("http tiene que ser ip:puerto"));
    }

    #[test]
    fn archivo_faltante_dice_cual_en_castellano() {
        let e = cargar_config(&r("/no/existe/rp.toml")).unwrap_err();
        assert_eq!(e.0, "no pude leer /no/existe/rp.toml: no existe");
    }

    const CASA: &str = "# casa\nPS5_HOST=consola.ejemplo.net\n";
    const PS5: &str = "PS5_REGIST_KEY=a1b2c3d4\nPS5_MORNING=000102030405060708090a0b0c0d0e0f\nPS5_ACCOUNT_ID=\"NDJvGJAjYVQ=\"\n";

    #[test]
    fn arma_el_perfil_desde_los_env() {
        let p = parsear_perfil(CASA, &r("casa.env"), PS5, &r("ps5.env"), 8000).unwrap();
        assert_eq!(p.host, "consola.ejemplo.net");
        assert_eq!(&p.regist_key[..8], b"a1b2c3d4");
        assert_eq!(&p.regist_key[8..], &[0u8; 8]);
        assert_eq!(p.morning, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        assert_eq!(p.account_id, [0x34, 0x32, 0x6f, 0x18, 0x90, 0x23, 0x61, 0x54]);
        assert_eq!(p.credencial_despertar(), Some(0xa1b2c3d4));
        assert_eq!(p.bitrate_kbps, 8000);
    }

    #[test]
    fn los_errores_del_perfil_no_muestran_valores() {
        let malo = PS5.replace("000102030405060708090a0b0c0d0e0f", "zz0102030405060708090a0b0c0d0e0f");
        let e = parsear_perfil(CASA, &r("casa.env"), &malo, &r("ps5.env"), 8000).unwrap_err();
        assert_eq!(e.0, "PS5_MORNING inválido en ps5.env: tienen que ser 32 caracteres hexadecimales");
        let sin_host = parsear_perfil("", &r("casa.env"), PS5, &r("ps5.env"), 8000).unwrap_err();
        assert_eq!(sin_host.0, "falta PS5_HOST en casa.env");
        let cuenta = PS5.replace("NDJvGJAjYVQ=", "AAAA");
        let e = parsear_perfil(CASA, &r("casa.env"), &cuenta, &r("ps5.env"), 8000).unwrap_err();
        assert!(e.0.starts_with("PS5_ACCOUNT_ID inválido"), "{}", e.0);
        assert!(!e.0.contains("AAAA"));
    }

    #[test]
    fn el_debug_del_perfil_oculta_los_secretos_y_el_host() {
        let p = parsear_perfil(CASA, &r("casa.env"), PS5, &r("ps5.env"), 8000).unwrap();
        let d = format!("{p:?}");
        assert!(!d.contains("consola"), "{d}");
        assert!(!d.contains("97") && !d.contains("a1b2"), "{d}");
    }

    #[test]
    fn argumentos() {
        let a = |v: &[&str]| parsear_argumentos(v.iter().map(|s| s.to_string()));
        assert_eq!(a(&[]).unwrap(), Argumentos::Correr(Rutas::default()));
        assert_eq!(
            a(&["--config", "/tmp/rp.toml", "--casa", "/tmp/casa.env"]).unwrap(),
            Argumentos::Correr(Rutas { config: r("/tmp/rp.toml"), casa: r("/tmp/casa.env"), ps5: r(RUTA_PS5) })
        );
        assert_eq!(a(&["--version"]).unwrap(), Argumentos::Version);
        assert!(a(&["--config"]).is_err());
        assert!(a(&["--otra"]).is_err());
    }
}

//! Métricas de la sesión para la salud (`GET /control/salud`): estado, cuadros por segundo y kbps de
//! los últimos 10 s, cuadros perdidos en esa ventana y la hora de la última conexión.
//! El ciclo las escribe y el servidor las lee. El tiempo de las ventanas es monótono (`Instant` de
//! tokio); solo la "última reconexión" usa la hora de pared, porque es una marca para mostrar.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tokio::time::Instant;

/// Ventana de las métricas de video.
pub const VENTANA: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EstadoSesion {
    Conectada,
    Conectando,
    /// Sin sesión: esperando el próximo intento.
    Caida,
    /// En pausa por `/control/reposo` (la PS5 en reposo), hasta `/control/encender`.
    Reposo,
    /// Alguien usa la consola en forma local (la PS5 cerró la sesión): en pausa hasta `/control/reconectar`
    /// (o `/control/encender`). En el JSON, `"tomada"`.
    #[serde(rename = "tomada")]
    TomadaLocal,
}

/// Lo que se publica en `VideoSalud.sesion` (los nombres los fija el tipo de la api).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FotoSesion {
    pub estado: EstadoSesion,
    pub fps: f64,
    pub kbps: u64,
    pub perdidos: u64,
    #[serde(rename = "ultimaReconexion")]
    pub ultima_reconexion: Option<String>,
}

struct Cuadro {
    t: Instant,
    bytes: usize,
    perdidos: u32,
}

struct Interno {
    estado: EstadoSesion,
    conectada_desde: Option<Instant>,
    cuadros: VecDeque<Cuadro>,
    ultima_reconexion: Option<String>,
}

pub struct Metricas {
    interno: Mutex<Interno>,
}

impl Default for Metricas {
    fn default() -> Self {
        Self::new()
    }
}

impl Metricas {
    pub fn new() -> Self {
        Self {
            interno: Mutex::new(Interno {
                estado: EstadoSesion::Caida,
                conectada_desde: None,
                cuadros: VecDeque::new(),
                ultima_reconexion: None,
            }),
        }
    }

    fn con<R>(&self, f: impl FnOnce(&mut Interno) -> R) -> R {
        f(&mut self.interno.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// La sesión pasó a `Conectando` o `Caida` (para `Conectada`, usar `conectada`).
    pub fn estado(&self, estado: EstadoSesion) {
        self.con(|m| {
            m.estado = estado;
            if estado != EstadoSesion::Conectada {
                m.conectada_desde = None;
                m.cuadros.clear();
            }
        });
    }

    /// La sesión se conectó: se anota la hora (`pared`) como última reconexión.
    pub fn conectada(&self, ahora: Instant, pared: SystemTime) {
        self.con(|m| {
            m.estado = EstadoSesion::Conectada;
            m.conectada_desde = Some(ahora);
            m.cuadros.clear();
            m.ultima_reconexion = Some(iso8601(pared));
        });
    }

    /// Un cuadro de la PS5 que salió (`perdidos`: los que se perdieron antes de este).
    pub fn cuadro(&self, ahora: Instant, bytes: usize, perdidos: u32) {
        self.con(|m| {
            if m.estado != EstadoSesion::Conectada {
                return;
            }
            m.cuadros.push_back(Cuadro { t: ahora, bytes, perdidos });
            podar(&mut m.cuadros, ahora);
        });
    }

    pub fn foto(&self, ahora: Instant) -> FotoSesion {
        self.con(|m| {
            podar(&mut m.cuadros, ahora);
            let (mut fps, mut kbps, mut perdidos) = (0.0, 0, 0);
            if let (EstadoSesion::Conectada, Some(desde)) = (m.estado, m.conectada_desde) {
                // Recién conectada, la ventana es el tiempo que lleva conectada (mínimo 1 s).
                let ventana = ahora.saturating_duration_since(desde).clamp(Duration::from_secs(1), VENTANA).as_secs_f64();
                let bytes: usize = m.cuadros.iter().map(|c| c.bytes).sum();
                fps = (m.cuadros.len() as f64 / ventana * 10.0).round() / 10.0;
                kbps = (bytes as f64 * 8.0 / 1000.0 / ventana).round() as u64;
                perdidos = m.cuadros.iter().map(|c| u64::from(c.perdidos)).sum();
            }
            FotoSesion { estado: m.estado, fps, kbps, perdidos, ultima_reconexion: m.ultima_reconexion.clone() }
        })
    }
}

/// Saca los cuadros de más de `VENTANA` de antigüedad.
fn podar(cuadros: &mut VecDeque<Cuadro>, ahora: Instant) {
    while cuadros.front().is_some_and(|c| ahora.saturating_duration_since(c.t) >= VENTANA) {
        cuadros.pop_front();
    }
}

/// Fecha y hora UTC en ISO 8601 (`2026-11-21T14:03:05Z`), sin depender de una biblioteca de fechas.
/// Un reloj anterior a 1970 se muestra como 1970-01-01.
pub fn iso8601(t: SystemTime) -> String {
    let s = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (dias, resto) = (s / 86_400, s % 86_400);
    // Días desde 1970 a fecha civil (algoritmo de Howard Hinnant, "civil_from_days").
    let z = dias as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let dia = doy - (153 * mp + 2) / 5 + 1;
    let mes = if mp < 10 { mp + 3 } else { mp - 9 };
    let anio = yoe + era * 400 + i64::from(mes <= 2);
    format!(
        "{anio:04}-{mes:02}-{dia:02}T{:02}:{:02}:{:02}Z",
        resto / 3_600,
        resto % 3_600 / 60,
        resto % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix(s: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(s)
    }

    #[test]
    fn iso8601_en_utc() {
        assert_eq!(iso8601(unix(0)), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(unix(1_800_000_000)), "2027-01-15T08:00:00Z");
        assert_eq!(iso8601(unix(951_782_400)), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601(unix(4_107_542_399)), "2100-02-28T23:59:59Z");
        // Antes de 1970 (reloj roto): no entra en pánico.
        assert_eq!(iso8601(SystemTime::UNIX_EPOCH - Duration::from_secs(5)), "1970-01-01T00:00:00Z");
    }

    #[tokio::test(start_paused = true)]
    async fn arranca_caida_y_sin_reconexion() {
        let m = Metricas::new();
        let f = m.foto(Instant::now());
        assert_eq!(f, FotoSesion { estado: EstadoSesion::Caida, fps: 0.0, kbps: 0, perdidos: 0, ultima_reconexion: None });
        m.estado(EstadoSesion::Conectando);
        assert_eq!(m.foto(Instant::now()).estado, EstadoSesion::Conectando);
    }

    #[tokio::test(start_paused = true)]
    async fn fps_kbps_y_perdidos_de_los_ultimos_10_s() {
        let m = Metricas::new();
        let t0 = Instant::now();
        m.conectada(t0, unix(1_800_000_000));
        // 20 s a 60 fps, 10 000 bytes por cuadro, 1 perdido cada 60.
        for k in 0..1200u64 {
            let t = t0 + Duration::from_micros(k * 16_667);
            m.cuadro(t, 10_000, u32::from(k % 60 == 0));
        }
        let f = m.foto(t0 + Duration::from_secs(20));
        assert_eq!(f.estado, EstadoSesion::Conectada);
        assert!((f.fps - 60.0).abs() <= 0.2, "{}", f.fps);
        // 600 cuadros × 80 000 bits / 10 s = 4800 kbps.
        assert!((4790..=4810).contains(&f.kbps), "{}", f.kbps);
        assert_eq!(f.perdidos, 10);
        assert_eq!(f.ultima_reconexion.as_deref(), Some("2027-01-15T08:00:00Z"));
    }

    #[tokio::test(start_paused = true)]
    async fn recien_conectada_divide_por_el_tiempo_conectada() {
        let m = Metricas::new();
        let t0 = Instant::now();
        m.conectada(t0, unix(0));
        for k in 0..120u64 {
            m.cuadro(t0 + Duration::from_micros(k * 16_667), 1_000, 0);
        }
        let f = m.foto(t0 + Duration::from_secs(2));
        assert!((f.fps - 60.0).abs() <= 0.5, "a los 2 s no marca 12 fps: {}", f.fps);
    }

    #[tokio::test(start_paused = true)]
    async fn al_caerse_vuelve_a_cero_pero_guarda_la_ultima_reconexion() {
        let m = Metricas::new();
        let t0 = Instant::now();
        m.conectada(t0, unix(1_800_000_000));
        m.cuadro(t0, 1_000, 3);
        m.estado(EstadoSesion::Caida);
        let f = m.foto(t0 + Duration::from_secs(1));
        assert_eq!((f.estado, f.fps, f.kbps, f.perdidos), (EstadoSesion::Caida, 0.0, 0, 0));
        assert_eq!(f.ultima_reconexion.as_deref(), Some("2027-01-15T08:00:00Z"));
    }

    #[tokio::test(start_paused = true)]
    async fn en_reposo_no_hay_video_y_se_informa_reposo() {
        let m = Metricas::new();
        let t0 = Instant::now();
        m.conectada(t0, unix(0));
        m.cuadro(t0, 1_000, 1);
        m.estado(EstadoSesion::Reposo);
        let f = m.foto(t0 + Duration::from_secs(1));
        assert_eq!((f.estado, f.fps, f.kbps, f.perdidos), (EstadoSesion::Reposo, 0.0, 0, 0));
        assert_eq!(serde_json::to_value(EstadoSesion::Reposo).unwrap(), serde_json::json!("reposo"));
    }

    #[test]
    fn la_consola_tomada_se_informa_como_tomada() {
        assert_eq!(serde_json::to_value(EstadoSesion::TomadaLocal).unwrap(), serde_json::json!("tomada"));
        assert_eq!(serde_json::to_value(EstadoSesion::Caida).unwrap(), serde_json::json!("caida"));
    }

    #[test]
    fn el_json_tiene_los_nombres_de_la_api() {
        let f = FotoSesion { estado: EstadoSesion::Conectando, fps: 59.9, kbps: 8000, perdidos: 2, ultima_reconexion: None };
        assert_eq!(
            serde_json::to_string(&f).unwrap(),
            r#"{"estado":"conectando","fps":59.9,"kbps":8000,"perdidos":2,"ultimaReconexion":null}"#
        );
    }
}

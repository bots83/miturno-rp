//! Registro de la sincronía de la PS5: cuánto se apartan del reloj las marcas que salen de audio y de
//! video, y cuánto tardan en llegar los paquetes de cada pista. Es solo para mirar: no toca ninguna marca.
//!
//! Cada `VENTANA_SINCRONIA` se cierra una ventana y, si hubo algo fuera de lo normal, sale un resumen de
//! una línea (ver `Resumen`). En régimen no se registra nada.

use std::fmt;
use std::time::Duration;

/// Largo de cada ventana de medición.
pub const VENTANA_SINCRONIA: Duration = Duration::from_secs(10);

/// Desfase (en cualquiera de las pistas, o de A/V) que ya no es normal: más de 50 ms.
const DESFASE_NORMAL: i64 = 90 * 50;
/// Hueco entre llegadas seguidas de una pista que ya no es normal: más de 100 ms.
const HUECO_NORMAL: Duration = Duration::from_millis(100);

/// Lo medido de una pista.
#[derive(Default, Clone, Copy)]
struct Pista {
    /// Desfase mínimo y máximo de la ventana, en ticks (`None`: sin paquetes en la ventana).
    rango: Option<(i64, i64)>,
    /// Último desfase de la ventana, para el A/V.
    ultimo: Option<i64>,
    /// Hueco más largo entre llegadas seguidas que terminó en la ventana.
    hueco: Duration,
    /// Última llegada: sobrevive al cierre de la ventana (un hueco que la cruza cuenta en la nueva).
    llegada: Option<Duration>,
}

impl Pista {
    fn anotar(&mut self, desfase: i64, ahora: Duration) {
        self.rango = Some(self.rango.map_or((desfase, desfase), |(a, b)| (a.min(desfase), b.max(desfase))));
        self.ultimo = Some(desfase);
        if let Some(antes) = self.llegada {
            self.hueco = self.hueco.max(ahora.saturating_sub(antes));
        }
        self.llegada = Some(ahora);
    }

    /// Empieza otra ventana: solo queda la última llegada.
    fn reiniciar(&mut self) {
        *self = Pista { llegada: self.llegada, ..Pista::default() };
    }

    fn fuera_de_lo_normal(&self) -> bool {
        let lejos = |(a, b): (i64, i64)| a.abs() > DESFASE_NORMAL || b.abs() > DESFASE_NORMAL;
        self.rango.is_some_and(lejos) || self.hueco > HUECO_NORMAL
    }
}

/// Desfase de las marcas de la PS5 contra el reloj y huecos entre llegadas, por ventanas.
#[derive(Default)]
pub struct Sincronia {
    video: Pista,
    audio: Pista,
    /// A/V (último desfase de video − último de audio) de mayor valor absoluto de la ventana, en ticks.
    av: i64,
    /// Comienzo de la ventana: la primera llegada después de crearla, de cerrarla o de `olvidar`.
    desde: Option<Duration>,
}

impl Sincronia {
    pub fn new() -> Self {
        Self::default()
    }

    /// Salió un cuadro de video con `desfase_ticks` = marca − reloj (90 kHz), en el instante `ahora`.
    pub fn video(&mut self, desfase_ticks: i64, ahora: Duration) {
        self.video.anotar(desfase_ticks, ahora);
        self.despues_de_anotar(ahora);
    }

    /// Salió un paquete de audio con `desfase_ticks` = marca − reloj (90 kHz), en el instante `ahora`.
    pub fn audio(&mut self, desfase_ticks: i64, ahora: Duration) {
        self.audio.anotar(desfase_ticks, ahora);
        self.despues_de_anotar(ahora);
    }

    fn despues_de_anotar(&mut self, ahora: Duration) {
        self.desde.get_or_insert(ahora);
        if let (Some(v), Some(a)) = (self.video.ultimo, self.audio.ultimo) {
            if (v - a).abs() > self.av.abs() {
                self.av = v - a;
            }
        }
    }

    /// Si ya pasó la ventana, la cierra (y empieza otra): devuelve el resumen solo si hubo algo fuera de
    /// lo normal.
    pub fn cerrar(&mut self, ahora: Duration) -> Option<Resumen> {
        let desde = self.desde?;
        if ahora.saturating_sub(desde) < VENTANA_SINCRONIA {
            return None;
        }
        let anormal =
            self.video.fuera_de_lo_normal() || self.audio.fuera_de_lo_normal() || self.av.abs() > DESFASE_NORMAL;
        let resumen = anormal.then(|| Resumen {
            video: self.video.rango.unwrap_or_default(),
            audio: self.audio.rango,
            av: self.av,
            hueco_video: self.video.hueco,
            hueco_audio: self.audio.hueco,
        });
        self.video.reiniciar();
        self.audio.reiniciar();
        self.av = 0;
        self.desde = Some(ahora);
        resumen
    }

    /// Volvió el cartel: lo medido no sirve para la próxima sesión (ni el hueco hasta ella).
    pub fn olvidar(&mut self) {
        *self = Self::default();
    }

    /// El desfase de audio más bajo de la ventana actual, en ticks.
    #[cfg(test)]
    pub fn audio_minimo(&self) -> Option<i64> {
        self.audio.rango.map(|r| r.0)
    }
}

/// Lo que dejó una ventana con algo fuera de lo normal (desfases en ticks de 90 kHz).
pub struct Resumen {
    video: (i64, i64),
    /// `None`: no salió audio en la ventana.
    audio: Option<(i64, i64)>,
    av: i64,
    hueco_video: Duration,
    hueco_audio: Duration,
}

/// Ticks de 90 kHz a milisegundos redondeados (con signo).
fn ms(ticks: i64) -> i64 {
    (ticks as f64 / 90.0).round() as i64
}

/// Una duración en milisegundos redondeados.
fn ms_de(d: Duration) -> u128 {
    (d.as_micros() + 500) / 1_000
}

impl fmt::Display for Resumen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let segundos = VENTANA_SINCRONIA.as_secs();
        let (vmin, vmax) = self.video;
        write!(f, "salida: últimos {segundos} s: video {}..{} ms del reloj, ", ms(vmin), ms(vmax))?;
        match self.audio {
            Some((amin, amax)) => write!(f, "audio {}..{} ms", ms(amin), ms(amax))?,
            None => write!(f, "audio sin paquetes")?,
        }
        write!(
            f,
            ", A/V hasta {} ms, hueco máximo de video {} ms, de audio {} ms",
            ms(self.av),
            ms_de(self.hueco_video),
            ms_de(self.hueco_audio)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ticks de 90 kHz para `n` milisegundos (con signo).
    fn t(n: i64) -> i64 {
        n * 90
    }

    fn us(n: u64) -> Duration {
        Duration::from_micros(n)
    }

    /// Simula `hasta_us` de PS5: video cada 16,667 ms y audio cada 20 ms, con los desfases que dé cada
    /// función según el instante (en µs). Llama a `cerrar` después de cada cuadro, como la salida, y
    /// devuelve los resúmenes.
    fn simular(
        s: &mut Sincronia,
        desde_us: u64,
        hasta_us: u64,
        dv: impl Fn(u64) -> i64,
        da: impl Fn(u64) -> i64,
        hueco_video: Option<(u64, u64)>,
    ) -> Vec<String> {
        let mut resumenes = Vec::new();
        let (mut sv, mut sa) = (desde_us, desde_us);
        while sv < hasta_us || sa < hasta_us {
            if sa <= sv {
                s.audio(da(sa), us(sa));
                sa += 20_000;
            } else {
                let calla = hueco_video.is_some_and(|(a, b)| sv >= a && sv < b);
                if !calla {
                    s.video(dv(sv), us(sv));
                    if let Some(r) = s.cerrar(us(sv)) {
                        resumenes.push(r.to_string());
                    }
                }
                sv += 16_667;
            }
        }
        resumenes
    }

    /// Un vaivén de ±10 ms según el instante.
    fn vaiven(x: u64) -> i64 {
        t(((x / 1_000) % 21) as i64 - 10)
    }

    #[test]
    fn en_regimen_no_hay_resumen() {
        let mut s = Sincronia::new();
        let r = simular(&mut s, 0, 30_000_000, vaiven, |x| -vaiven(x), None);
        assert!(r.is_empty(), "{r:?}");
    }

    #[test]
    fn audio_atrasado_300_ms_da_resumen() {
        let mut s = Sincronia::new();
        let r = simular(&mut s, 0, 10_500_000, |_| 0, |_| t(-300), None);
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0].starts_with("salida: últimos 10 s: video 0..0 ms del reloj, "), "{}", r[0]);
        assert!(r[0].contains("audio -300..-300 ms"), "{}", r[0]);
        assert!(r[0].contains("A/V hasta 300 ms"), "{}", r[0]);
        assert!(r[0].ends_with("hueco máximo de video 17 ms, de audio 20 ms"), "{}", r[0]);
    }

    #[test]
    fn el_texto_es_exacto() {
        let mut s = Sincronia::new();
        s.video(t(-5), us(0));
        s.audio(t(-80), us(0));
        s.video(t(12), us(16_667));
        s.audio(t(-60), us(20_000));
        s.video(t(3), us(33_333));
        assert!(s.cerrar(us(9_999_999)).is_none(), "la ventana todavía no pasó");
        let r = s.cerrar(us(10_000_000)).expect("audio a −80 ms: fuera de lo normal");
        assert_eq!(
            r.to_string(),
            "salida: últimos 10 s: video -5..12 ms del reloj, audio -80..-60 ms, A/V hasta 92 ms, \
             hueco máximo de video 17 ms, de audio 20 ms"
        );
    }

    #[test]
    fn sin_audio_en_la_ventana_lo_dice() {
        let mut s = Sincronia::new();
        s.video(t(70), us(0));
        s.video(t(70), us(16_667));
        let r = s.cerrar(us(10_000_000)).expect("video a 70 ms: fuera de lo normal");
        assert_eq!(
            r.to_string(),
            "salida: últimos 10 s: video 70..70 ms del reloj, audio sin paquetes, A/V hasta 0 ms, \
             hueco máximo de video 17 ms, de audio 0 ms"
        );
    }

    #[test]
    fn hueco_de_video_de_250_ms_da_resumen() {
        let mut s = Sincronia::new();
        // El video calla de 3 s a 3,25 s: entre la última llegada antes y la primera después hay ~267 ms.
        let r = simular(&mut s, 0, 10_500_000, |_| 0, |_| 0, Some((3_000_000, 3_250_000)));
        assert_eq!(r.len(), 1, "{r:?}");
        let hueco: i64 = r[0]
            .split("hueco máximo de video ")
            .nth(1)
            .and_then(|x| x.split(' ').next())
            .and_then(|x| x.parse().ok())
            .unwrap_or_else(|| panic!("{}", r[0]));
        assert!((250..=270).contains(&hueco), "{}", r[0]);
        assert!(r[0].contains("A/V hasta 0 ms"), "{}", r[0]);
    }

    #[test]
    fn un_hueco_de_100_ms_justos_es_normal() {
        // Desfases de ±50 ms, A/V de ±50 ms y un hueco de 100 ms: todo en el límite, todavía normal.
        let mut s = Sincronia::new();
        s.audio(0, us(0));
        s.video(t(50), us(0));
        s.video(t(-50), us(100_000));
        s.audio(t(-50), us(20_000));
        s.video(0, us(116_667));
        assert!(s.cerrar(us(10_000_000)).is_none(), "en el límite todavía es normal");
        // Un hueco de 100,001 ms ya no.
        let mut s = Sincronia::new();
        s.video(0, us(0));
        s.video(0, us(100_001));
        assert!(s.cerrar(us(10_000_000)).is_some());
    }

    #[test]
    fn una_linea_por_ventana() {
        let mut s = Sincronia::new();
        let r = simular(&mut s, 0, 31_000_000, |_| 0, |_| t(-300), None);
        assert_eq!(r.len(), 3, "{r:?}");
    }

    #[test]
    fn el_hueco_entre_ventanas_cuenta_en_la_nueva() {
        let mut s = Sincronia::new();
        s.video(0, us(0));
        assert!(s.cerrar(us(10_000_000)).is_none());
        s.video(0, us(10_300_000));
        let r = s.cerrar(us(20_300_000)).expect("hueco de 10,3 s");
        assert!(r.to_string().contains("hueco máximo de video 10300 ms"), "{r}");
    }

    #[test]
    fn olvidar_reinicia_sin_resumen() {
        let mut s = Sincronia::new();
        // 5 s con audio atrasado 300 ms, vuelve el cartel y después la PS5 en régimen.
        assert!(simular(&mut s, 0, 5_000_000, |_| 0, |_| t(-300), None).is_empty());
        s.olvidar();
        assert_eq!(s.audio_minimo(), None);
        // La nueva sesión arranca 20 s después: el hueco con la anterior no cuenta.
        let r = simular(&mut s, 25_000_000, 50_000_000, vaiven, vaiven, None);
        assert!(r.is_empty(), "{r:?}");
    }
}

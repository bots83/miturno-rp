//! La salida de video: un solo MPEG-TS continuo hacia MediaMTX, con marcas de tiempo propias.
//!
//! - Sin sesión (o mientras se espera el primer keyframe de la PS5) repite el cartel en bucle.
//! - Con la PS5: empalma en el primer keyframe (IDR) que llega y desde ahí pasa sus cuadros tal cual.
//! - Si la sesión se cae, o la PS5 deja de mandar video, vuelve al cartel en el acto (cada vuelta del
//!   cartel empieza con su keyframe).
//!
//! Las marcas de tiempo (PTS) siguen un reloj monótono que pasa quien llama (`ahora`, desde el arranque)
//! y nunca retroceden, tampoco en los cambios; así Kick y los reproductores nunca ven un corte.
//! El origen se toma una sola vez al arrancar, de la hora del sistema (`origen_de`): si miturno-rp se
//! reinicia con Kick al aire, las marcas siguen hacia adelante en vez de volver a empezar (con un origen
//! fijo saltaban hacia atrás y Kick quedaba congelado varios segundos). Adentro todo es u64 sin recortar;
//! el mux recorta PTS y PCR a 33 bits, así que la vuelta de los 33 bits (~26,5 h) pasa sin saltos.

use std::borrow::Cow;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cartel::Cartel;
use crate::mux::{duracion_opus, partir_annexb, tipo_nal, Destino, Mux, CUADRO, HZ};

/// Largo del reloj de MPEG-TS: PTS y PCR son de 33 bits.
const VUELTA_33: u64 = 1 << 33;
/// Sin video de la PS5 por este tiempo, se vuelve al cartel.
pub const PS5_TRABADA: Duration = Duration::from_millis(1_500);
/// Si el cartel se atrasó más que esto (el proceso estuvo trabado), reempieza la vuelta en vez de
/// mandar la ráfaga de cuadros atrasados.
const ATRASO_CARTEL: u64 = HZ / 2;
/// Corrección de la cuenta de la PS5 contra el reloj: dentro de ±50 ms no se toca; más allá se corre
/// un 1 % por paquete (un 10 % si pasa de 500 ms adelantada); si quedó más de 500 ms atrás, salta al reloj.
const BANDA: u64 = HZ / 20;
const LEJOS: u64 = HZ / 2;

/// Origen de las marcas para un arranque en `t`: la hora del sistema en marcas de 90 kHz, en 33 bits.
pub fn origen_de(t: SystemTime) -> u64 {
    let desde_1970 = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (desde_1970.as_nanos() * 9 / 100_000) as u64 & (VUELTA_33 - 1)
}

/// Origen de las marcas para un arranque ahora.
pub fn origen_del_reloj() -> u64 {
    origen_de(SystemTime::now())
}

/// Siguiente marca de una cuenta (`esperada`) corregida contra el reloj (`reloj`), sin bajar de `minima`.
fn ajustar(esperada: u64, reloj: u64, paso: u64, minima: u64) -> u64 {
    let m = if esperada + LEJOS < reloj {
        reloj
    } else if esperada > reloj + LEJOS {
        esperada - paso / 10
    } else if esperada > reloj + BANDA {
        esperada - paso / 100
    } else if esperada + BANDA < reloj {
        esperada + paso / 100
    } else {
        esperada
    };
    m.max(minima)
}

enum Modo {
    /// Repitiendo el cartel desde `base`; `v` cuadros y `a` paquetes de audio ya emitidos.
    Cartel { base: u64, v: u64, a: u64 },
    /// Pasando la PS5. `audio_nuevo`: el próximo paquete de audio es el primero después del cambio.
    Ps5 { ultimo_video: Duration, audio_nuevo: bool },
}

pub struct Salida<D: Destino> {
    mux: Mux<D>,
    /// Marca que corresponde a `ahora = 0` (ver `origen_de`).
    origen: u64,
    cartel: Cartel,
    modo: Modo,
    /// Próximas marcas esperadas (último + duración). Ninguna marca nueva baja de acá: así nunca retroceden.
    v_sig: u64,
    a_sig: u64,
    /// Duración de un paquete de audio de la PS5 según su encabezado (si el TOC no alcanza).
    dur_audio_ps5: u64,
    /// SPS y PPS de la PS5, para ponerlos delante de un keyframe que llegue sin ellos.
    parametros: Vec<u8>,
}

impl<D: Destino> Salida<D> {
    /// Arranca mostrando el cartel desde `ahora = 0`, que en las marcas es `origen` (en producción,
    /// `origen_del_reloj()`).
    pub fn new(cartel: Cartel, destino: D, origen: u64) -> Self {
        assert!(!cartel.video.is_empty(), "cartel sin video");
        Self {
            mux: Mux::new(destino),
            origen,
            cartel,
            modo: Modo::Cartel { base: origen, v: 0, a: 0 },
            v_sig: 0,
            a_sig: 0,
            dur_audio_ps5: 480 * HZ / 48_000,
            parametros: Vec::new(),
        }
    }

    pub fn destino(&self) -> &D {
        self.mux.destino()
    }

    /// Marca de 90 kHz para un instante (desde el arranque de la salida).
    fn marcas(&self, ahora: Duration) -> u64 {
        self.origen + (ahora.as_nanos() * 9 / 100_000) as u64
    }

    /// El primer instante en el que `marcas(instante) >= marca`.
    fn instante(&self, marca: u64) -> Duration {
        let nanos = (marca.saturating_sub(self.origen) as u128 * 100_000).div_ceil(9);
        Duration::from_nanos(nanos as u64)
    }

    fn a_cartel(&mut self, ahora: Duration) {
        let base = self.marcas(ahora).max(self.v_sig).max(self.a_sig);
        self.modo = Modo::Cartel { base, v: 0, a: 0 };
    }

    fn proximas_del_cartel(&self, base: u64, v: u64, a: u64) -> (u64, u64) {
        let pv = base + v * CUADRO;
        let n = self.cartel.audio.len() as u64;
        let pa = if n == 0 { u64::MAX } else { base + (a / n) * self.cartel.duracion() + self.cartel.audio[(a % n) as usize].0 };
        (pv, pa)
    }

    /// Emite lo que toque del cartel y vuelve a él si la PS5 dejó de mandar video.
    pub fn tick(&mut self, ahora: Duration) {
        if let Modo::Ps5 { ultimo_video, .. } = self.modo {
            if ahora.saturating_sub(ultimo_video) < PS5_TRABADA {
                return;
            }
            crate::aviso!("salida: la PS5 no manda video hace {:?}; vuelve el cartel", PS5_TRABADA);
            self.a_cartel(ahora);
        }
        let reloj = self.marcas(ahora);
        if let Modo::Cartel { base, v, .. } = self.modo {
            if base + v * CUADRO + ATRASO_CARTEL < reloj {
                self.a_cartel(ahora);
            }
        }
        while let Modo::Cartel { base, v, a } = self.modo {
            let (pv, pa) = self.proximas_del_cartel(base, v, a);
            if pv.min(pa) > reloj {
                break;
            }
            let r = if pv <= pa {
                let i = (v % self.cartel.video.len() as u64) as usize;
                self.v_sig = pv + CUADRO;
                self.modo = Modo::Cartel { base, v: v + 1, a };
                self.mux.video(&self.cartel.video[i], pv, i == 0)
            } else {
                let (_, p) = &self.cartel.audio[(a % self.cartel.audio.len() as u64) as usize];
                self.a_sig = pa + duracion_opus(p).unwrap_or(0);
                self.modo = Modo::Cartel { base, v, a: a + 1 };
                self.mux.audio(p, pa)
            };
            if let Err(e) = r {
                crate::falla!("salida: no pude escribir el cartel: {e}");
            }
        }
    }

    /// Cuándo hay que volver a llamar a `tick`.
    pub fn proximo(&self) -> Duration {
        match self.modo {
            Modo::Cartel { base, v, a } => {
                let (pv, pa) = self.proximas_del_cartel(base, v, a);
                self.instante(pv.min(pa))
            }
            Modo::Ps5 { ultimo_video, .. } => ultimo_video + PS5_TRABADA,
        }
    }

    /// Mostrando el cartel: hace falta un keyframe de la PS5 para volver al juego.
    pub fn en_cartel(&self) -> bool {
        matches!(self.modo, Modo::Cartel { .. })
    }

    pub fn video_ps5(&mut self, au: &[u8], perdidos: u32, ahora: Duration) {
        let (mut vcl, mut clave, mut nuevos) = (false, false, Vec::new());
        for n in partir_annexb(au) {
            match tipo_nal(n) {
                Some(1..=4) => vcl = true,
                Some(5) => (vcl, clave) = (true, true),
                Some(7 | 8) => nuevos.extend_from_slice(n),
                _ => {}
            }
        }
        let trae_parametros = !nuevos.is_empty();
        if trae_parametros {
            self.parametros = nuevos;
        }
        // Una muestra con solo SPS/PPS (el encabezado del stream) se guarda y no sale.
        if !vcl {
            return;
        }
        let reloj = self.marcas(ahora);
        let pts = match self.modo {
            Modo::Cartel { .. } => {
                if !clave {
                    return;
                }
                crate::info!("salida: llegó un keyframe de la PS5; sale el cartel");
                reloj.max(self.v_sig)
            }
            Modo::Ps5 { .. } => {
                let esperada = self.v_sig + perdidos as u64 * CUADRO;
                ajustar(esperada, reloj, CUADRO, self.v_sig.saturating_sub(CUADRO) + 1)
            }
        };
        let audio_nuevo = match self.modo {
            Modo::Cartel { .. } => true,
            Modo::Ps5 { audio_nuevo, .. } => audio_nuevo,
        };
        self.modo = Modo::Ps5 { ultimo_video: ahora, audio_nuevo };
        let datos: Cow<[u8]> = if clave && !trae_parametros && !self.parametros.is_empty() {
            Cow::Owned([self.parametros.as_slice(), au].concat())
        } else {
            Cow::Borrowed(au)
        };
        self.v_sig = pts + CUADRO;
        if let Err(e) = self.mux.video(&datos, pts, clave) {
            crate::falla!("salida: no pude escribir un cuadro de la PS5: {e}");
        }
    }

    pub fn audio_ps5(&mut self, opus: &[u8], ahora: Duration) {
        let Modo::Ps5 { ultimo_video, audio_nuevo } = self.modo else { return };
        if opus.is_empty() {
            // Paquete perdido: la línea de tiempo sigue.
            self.a_sig += self.dur_audio_ps5;
            return;
        }
        let dur = duracion_opus(opus).unwrap_or(self.dur_audio_ps5);
        let reloj = self.marcas(ahora);
        let pts = if audio_nuevo {
            reloj.max(self.a_sig)
        } else {
            let minima = self.a_sig.saturating_sub(dur) + 1;
            ajustar(self.a_sig, reloj, dur, minima)
        };
        self.modo = Modo::Ps5 { ultimo_video, audio_nuevo: false };
        self.a_sig = pts + dur;
        if let Err(e) = self.mux.audio(opus, pts) {
            crate::falla!("salida: no pude escribir audio de la PS5: {e}");
        }
    }

    pub fn formato_audio(&mut self, frecuencia: u32, muestras_por_paquete: u32) {
        if frecuencia > 0 && muestras_por_paquete > 0 {
            self.dur_audio_ps5 = muestras_por_paquete as u64 * HZ / frecuencia as u64;
        }
    }

    /// La sesión terminó: vuelve el cartel ya (si no estaba).
    pub fn sesion_caida(&mut self, ahora: Duration) {
        self.parametros.clear();
        if !self.en_cartel() {
            self.a_cartel(ahora);
            self.tick(ahora);
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::mux::tests::leer_pes;
    use crate::mux::{ID_AUDIO, ID_VIDEO};

    /// Cartel de prueba: 6 cuadros (100 ms: keyframe + 5 P, marcados con 0xCA) y 5 paquetes Opus de 20 ms.
    pub fn cartel_de_prueba() -> Cartel {
        let mut video = vec![vec![0, 0, 0, 1, 0x65, 0xCA, 0]];
        video.extend((1..6).map(|k| vec![0, 0, 0, 1, 0x41, 0xCA, k]));
        Cartel { video, audio: (0..5).map(|k| (k * 1800, vec![0xFC, 0xCA, k as u8])).collect() }
    }

    const SPS_PPS: [u8; 10] = [0, 0, 0, 1, 0x67, 0x50, 0, 0, 1, 0x68];
    fn idr_ps5(n: u8) -> Vec<u8> {
        vec![0, 0, 0, 1, 0x65, 0x50, n]
    }
    fn p_ps5(n: u8) -> Vec<u8> {
        vec![0, 0, 0, 1, 0x41, 0x50, n]
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Llama a `tick` cada 1 ms de `desde` a `hasta` (sin incluir).
    fn ticks(s: &mut Salida<Vec<u8>>, desde: u64, hasta: u64) {
        for t in desde..hasta {
            s.tick(ms(t));
        }
    }

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Fuente {
        Cartel,
        Ps5,
    }

    fn fuente(datos: &[u8]) -> Fuente {
        if datos.contains(&0xCA) {
            Fuente::Cartel
        } else if datos.contains(&0x50) {
            Fuente::Ps5
        } else {
            panic!("carga desconocida {datos:?}")
        }
    }

    fn es_clave(au: &[u8]) -> bool {
        crate::mux::partir_annexb(au).iter().any(|n| crate::mux::tipo_nal(n) == Some(5))
    }

    /// (PTS, fuente, keyframe) de cada cuadro de video que salió.
    fn videos(s: &Salida<Vec<u8>>) -> Vec<(u64, Fuente, bool)> {
        leer_pes(s.destino())
            .into_iter()
            .filter(|p| p.0 == ID_VIDEO)
            .map(|(_, pts, d)| (pts, fuente(&d), es_clave(&d)))
            .collect()
    }

    fn audios(s: &Salida<Vec<u8>>) -> Vec<(u64, Fuente)> {
        leer_pes(s.destino()).into_iter().filter(|p| p.0 == ID_AUDIO).map(|(_, pts, d)| (pts, fuente(&d))).collect()
    }

    fn crecientes(pts: &[u64]) -> bool {
        pts.windows(2).all(|w| w[1] > w[0])
    }

    /// Origen de las pruebas: 1 s, como antes de tomarlo de la hora del sistema.
    pub const ORIGEN_PRUEBA: u64 = HZ;
    const VUELTA: u64 = 1 << 33;

    #[test]
    fn el_origen_sale_de_la_hora_del_sistema_en_33_bits() {
        use std::time::{SystemTime, UNIX_EPOCH};
        assert_eq!(origen_de(UNIX_EPOCH + Duration::from_secs(1)), HZ);
        assert_eq!(origen_de(UNIX_EPOCH + Duration::from_millis(1_500)), HZ + HZ / 2);
        // 2^33 marcas son ~26,5 h: a las 2^33 + 7 marcas desde 1970 da 7.
        let t = UNIX_EPOCH + Duration::from_nanos((VUELTA + 7) * 100_000 / 9 + 1);
        assert_eq!(origen_de(t), 7);
        // Dos arranques seguidos dan orígenes crecientes (salvo justo en la vuelta, cada ~26,5 h).
        let a = origen_de(SystemTime::now());
        let b = origen_de(SystemTime::now() + Duration::from_secs(5));
        assert!(b.wrapping_sub(a) % VUELTA >= 5 * HZ - 1 && a < VUELTA && b < VUELTA, "{a} → {b}");
    }

    #[test]
    fn las_marcas_arrancan_en_el_origen_que_se_pasa() {
        let mut s = Salida::new(cartel_de_prueba(), Vec::new(), 123_456_789);
        s.tick(ms(0));
        s.tick(ms(20));
        let v = videos(&s);
        assert_eq!(v[0].0, 123_456_789);
        assert_eq!(audios(&s)[0].0, 123_456_789);
        assert_eq!(s.proximo(), Duration::from_nanos((2 * CUADRO * 100_000).div_ceil(9)));
    }

    #[test]
    fn a_un_segundo_de_la_vuelta_de_33_bits_las_marcas_siguen_creciendo_modulo_2_33() {
        let mut s = Salida::new(cartel_de_prueba(), Vec::new(), VUELTA - HZ);
        ticks(&mut s, 0, 500);
        s.video_ps5(&SPS_PPS, 0, ms(500));
        s.video_ps5(&idr_ps5(0), 0, ms(500));
        for k in 1..100u64 {
            s.tick(ms(500 + k * 16));
            s.video_ps5(&p_ps5(k as u8), 0, ms(500 + k * 16));
            s.audio_ps5(&[0xF4, 0x50], ms(500 + k * 16));
        }
        s.sesion_caida(ms(2_100));
        ticks(&mut s, 2_100, 2_500);
        let mod_crecientes = |pts: &[u64]| {
            pts.iter().all(|&p| p < VUELTA)
                && pts.windows(2).all(|w| {
                    // Un retroceso daría casi 2^33; los saltos hacia adelante de `ajustar` son de menos de 1 s.
                    let d = w[1].wrapping_sub(w[0]) % VUELTA;
                    d > 0 && d < HZ
                })
        };
        let v: Vec<u64> = videos(&s).iter().map(|c| c.0).collect();
        let a: Vec<u64> = audios(&s).iter().map(|c| c.0).collect();
        assert!(v.first().unwrap() > &(VUELTA - HZ - 1) && v.last().unwrap() < &(2 * HZ), "pasó la vuelta: {v:?}");
        assert!(a.last().unwrap() < &(2 * HZ), "el audio también pasó la vuelta");
        assert!(mod_crecientes(&v), "video: {v:?}");
        assert!(mod_crecientes(&a), "audio: {a:?}");
        let f: Vec<Fuente> = videos(&s).iter().map(|c| c.1).collect();
        assert!(f.contains(&Fuente::Ps5) && f.last() == Some(&Fuente::Cartel));
    }

    #[test]
    fn sin_sesion_emite_el_cartel_en_bucle_con_pts_crecientes() {
        let mut s = Salida::new(cartel_de_prueba(), Vec::new(), ORIGEN_PRUEBA);
        assert!(s.en_cartel());
        ticks(&mut s, 0, 200);
        let v = videos(&s);
        // 200 ms a 60 fps: 12 cuadros, el primero ya en t = 0 y keyframe.
        assert_eq!(v.len(), 12);
        assert!(v[0].2, "la primera vuelta empieza con el keyframe");
        assert!(v.iter().all(|c| c.1 == Fuente::Cartel));
        let pts: Vec<u64> = v.iter().map(|c| c.0).collect();
        assert!(pts.windows(2).all(|w| w[1] - w[0] == CUADRO), "{pts:?}");
        // Cada vuelta (6 cuadros) empieza con el keyframe.
        assert_eq!(v.iter().map(|c| c.2).collect::<Vec<_>>(), [true, false, false, false, false, false].repeat(2));
        let a = audios(&s);
        assert_eq!(a.len(), 10, "un paquete cada 20 ms");
        assert!(crecientes(&a.iter().map(|x| x.0).collect::<Vec<_>>()));
        // Audio y video arrancan juntos.
        assert_eq!(a[0].0, v[0].0);
    }

    #[test]
    fn el_cartel_arranca_enseguida_y_proximo_avisa_cuando_toca() {
        let mut s = Salida::new(cartel_de_prueba(), Vec::new(), ORIGEN_PRUEBA);
        assert_eq!(s.proximo(), Duration::ZERO);
        s.tick(Duration::ZERO);
        assert_eq!(videos(&s).len(), 1);
        assert!(s.proximo() > Duration::ZERO && s.proximo() <= Duration::from_micros(16_667));
    }

    #[test]
    fn video_de_la_ps5_sin_keyframe_sigue_el_cartel() {
        let mut s = Salida::new(cartel_de_prueba(), Vec::new(), ORIGEN_PRUEBA);
        ticks(&mut s, 0, 100);
        for k in 0..5u64 {
            s.video_ps5(&p_ps5(k as u8), 0, ms(100 + k * 16));
            s.audio_ps5(&[0xF4, 0x50], ms(100 + k * 16));
        }
        ticks(&mut s, 100, 200);
        assert!(s.en_cartel());
        assert!(videos(&s).iter().all(|c| c.1 == Fuente::Cartel));
        assert!(audios(&s).iter().all(|c| c.1 == Fuente::Cartel));
        assert_eq!(videos(&s).len(), 12, "el cartel no se interrumpió");
    }

    #[test]
    fn al_llegar_el_keyframe_cambia_a_la_ps5() {
        let mut s = Salida::new(cartel_de_prueba(), Vec::new(), ORIGEN_PRUEBA);
        ticks(&mut s, 0, 100);
        s.video_ps5(&SPS_PPS, 0, ms(100)); // solo parámetros: se guardan, no salen
        s.video_ps5(&p_ps5(0), 0, ms(101));
        s.video_ps5(&idr_ps5(1), 0, ms(102));
        assert!(!s.en_cartel());
        for k in 1..10u64 {
            s.tick(ms(102 + k * 16));
            s.video_ps5(&p_ps5(1 + k as u8), 0, ms(102 + k * 16));
            s.audio_ps5(&[0xF4, 0x50], ms(102 + k * 16));
        }
        let v = videos(&s);
        let primero_ps5 = v.iter().position(|c| c.1 == Fuente::Ps5).unwrap();
        assert!(v[primero_ps5].2, "el primer cuadro de la PS5 es el keyframe");
        assert!(v[primero_ps5..].iter().all(|c| c.1 == Fuente::Ps5), "después del cambio no hay más cartel");
        assert_eq!(v.len() - primero_ps5, 10);
        assert!(audios(&s).iter().any(|c| c.1 == Fuente::Ps5));
    }

    #[test]
    fn el_keyframe_sin_parametros_lleva_los_guardados() {
        let mut s = Salida::new(cartel_de_prueba(), Vec::new(), ORIGEN_PRUEBA);
        s.tick(ms(0));
        s.video_ps5(&SPS_PPS, 0, ms(5));
        s.video_ps5(&idr_ps5(1), 0, ms(10));
        let pes = leer_pes(s.destino());
        let ultimo = &pes.iter().rev().find(|p| p.0 == ID_VIDEO).unwrap().2;
        let mut esperado = SPS_PPS.to_vec();
        esperado.extend(idr_ps5(1));
        assert_eq!(ultimo, &esperado);
    }

    #[test]
    fn los_pts_nunca_retroceden_en_los_dos_cambios() {
        let mut s = Salida::new(cartel_de_prueba(), Vec::new(), ORIGEN_PRUEBA);
        ticks(&mut s, 0, 50);
        // La PS5 llega "adelantada" (cuenta cuadros perdidos) y después se cae.
        s.video_ps5(&idr_ps5(0), 0, ms(50));
        for k in 1..30u64 {
            s.tick(ms(50 + k * 16));
            s.video_ps5(&p_ps5(k as u8), if k == 10 { 3 } else { 0 }, ms(50 + k * 16));
            s.audio_ps5(&[0xF4, 0x50], ms(50 + k * 16));
            s.audio_ps5(&[0xF4, 0x50], ms(58 + k * 16));
        }
        s.sesion_caida(ms(530));
        assert!(s.en_cartel());
        ticks(&mut s, 530, 900);
        // Vuelve otra sesión.
        s.video_ps5(&idr_ps5(100), 0, ms(900));
        for k in 1..10u64 {
            s.tick(ms(900 + k * 16));
            s.video_ps5(&p_ps5(100 + k as u8), 0, ms(900 + k * 16));
            s.audio_ps5(&[0xF4, 0x50], ms(900 + k * 16));
        }
        let v = videos(&s);
        let fuentes: Vec<Fuente> = v.iter().map(|c| c.1).collect();
        let cambios = fuentes.windows(2).filter(|w| w[0] != w[1]).count();
        assert_eq!(cambios, 3, "cartel → PS5 → cartel → PS5");
        assert!(crecientes(&v.iter().map(|c| c.0).collect::<Vec<_>>()), "PTS de video: {v:?}");
        let a = audios(&s);
        assert!(crecientes(&a.iter().map(|c| c.0).collect::<Vec<_>>()), "PTS de audio: {a:?}");
        // Al volver al cartel, la vuelta empieza por su keyframe.
        let vuelta = fuentes.windows(2).position(|w| w == [Fuente::Ps5, Fuente::Cartel]).unwrap() + 1;
        assert!(v[vuelta].2);
    }

    #[test]
    fn el_audio_no_se_superpone_al_volver_al_cartel() {
        let mut s = Salida::new(cartel_de_prueba(), Vec::new(), ORIGEN_PRUEBA);
        s.tick(ms(0));
        s.video_ps5(&idr_ps5(0), 0, ms(1));
        for k in 0..20u64 {
            s.audio_ps5(&[0xF4, 0x50], ms(1 + k * 10)); // 10 ms cada uno
        }
        s.sesion_caida(ms(200));
        ticks(&mut s, 200, 300);
        let a = audios(&s);
        let ultimo_ps5 = a.iter().rev().find(|x| x.1 == Fuente::Ps5).unwrap().0;
        let primero_cartel = a.iter().find(|x| x.1 == Fuente::Cartel && x.0 > ultimo_ps5).unwrap().0;
        assert!(primero_cartel >= ultimo_ps5 + 900, "{ultimo_ps5} → {primero_cartel}");
    }

    #[test]
    fn si_la_ps5_deja_de_mandar_video_vuelve_al_cartel() {
        let mut s = Salida::new(cartel_de_prueba(), Vec::new(), ORIGEN_PRUEBA);
        s.tick(ms(0));
        s.video_ps5(&idr_ps5(0), 0, ms(10));
        assert!(!s.en_cartel());
        ticks(&mut s, 11, 1_400);
        assert!(!s.en_cartel(), "1,4 s sin video todavía no");
        ticks(&mut s, 1_400, 1_600);
        assert!(s.en_cartel(), "a los 1,5 s sin video vuelve al cartel");
        let v = videos(&s);
        assert_eq!(v.last().unwrap().1, Fuente::Cartel);
    }

    #[test]
    fn si_se_atrasa_mucho_el_cartel_reempieza_sin_rafagas() {
        let mut s = Salida::new(cartel_de_prueba(), Vec::new(), ORIGEN_PRUEBA);
        s.tick(ms(0));
        s.tick(ms(5_000)); // el proceso estuvo trabado 5 s
        let v = videos(&s);
        assert!(v.len() <= 3, "no manda 300 cuadros de golpe: {}", v.len());
        assert!(v.last().unwrap().2 || v.len() == 1);
    }
}

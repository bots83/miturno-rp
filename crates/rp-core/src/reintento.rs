//! Cuánto esperar entre intentos de conexión con la PS5, según por qué falló el anterior.
//!
//! - `Red` (corte de red, PS5 que no responde o sin video): 1, 2, 4 y después 5 s fijos. Se arregla
//!   sola y cada segundo de espera es cartel al aire, así que el tope es corto.
//! - `EnUso` ("Remote is already in use"): 3 s fijos y sin despertar. La PS5 está despierta y todavía
//!   tiene la sesión anterior; la rechaza en el pedido HTTP inicial (antes de abrir el control o el
//!   video), así que reintentar seguido no le cuesta nada (el cliente de chiaki-ng reintenta cada pocos ms).
//!   Después de `EN_USO_ANTES_DE_ESPACIAR` seguidos (unos 2 min), cada 15 s: si la PS5 quedó trabada con
//!   otra sesión, cada intento son varias líneas de registro y a 3 s llenaría el journal.
//! - `Otra` (lo que no se arregla solo: versión, PIN, Remote Play caído en la consola, el hilo de
//!   libchiaki muerto): 1, 2, 4, 8, 16 y después 30 s, para no martillar algo que necesita una persona.
//!
//! `Red` y `Otra` tienen cada una su escalón: muchas `Red` seguidas no hacen esperar 30 s al primer
//! `Otra`, y muchas `Otra` no suben la espera de la red.

use std::time::Duration;

pub const ESPERA_INICIAL: Duration = Duration::from_secs(1);
/// Tope con `Red`.
pub const ESPERA_MAXIMA: Duration = Duration::from_secs(5);
/// Fija con `EnUso`.
pub const ESPERA_EN_USO: Duration = Duration::from_secs(3);
/// Con `EnUso` después de `EN_USO_ANTES_DE_ESPACIAR` seguidos.
pub const ESPERA_EN_USO_ESPACIADA: Duration = Duration::from_secs(15);
/// Cuántos `EnUso` seguidos se reintentan cada 3 s (40 × 3 s = 2 min, más lo que tarda cada intento).
pub const EN_USO_ANTES_DE_ESPACIAR: u32 = 40;
/// Tope con `Otra`.
pub const ESPERA_MAXIMA_OTRA: Duration = Duration::from_secs(30);

/// Por qué hay que reintentar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Causa {
    EnUso,
    Red,
    Otra,
}

/// La espera hasta el próximo intento y si antes hay que mandar el paquete de encendido.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Espera {
    pub dura: Duration,
    pub despertar: bool,
}

/// El backoff: un escalón para `Red` y otro para `Otra` (cada uno con su tope) y la racha de `EnUso`
/// seguidos. `EnUso` no mueve los escalones; `Red` y `Otra` cortan la racha.
#[derive(Debug, Clone)]
pub struct Reintentos {
    red: Duration,
    otra: Duration,
    en_uso_seguidos: u32,
}

impl Default for Reintentos {
    fn default() -> Self {
        Self::new()
    }
}

impl Reintentos {
    pub fn new() -> Self {
        Self { red: ESPERA_INICIAL, otra: ESPERA_INICIAL, en_uso_seguidos: 0 }
    }

    /// Vuelve las esperas a 1 s y corta la racha de `EnUso` (sesión estable o reintento pedido a mano).
    pub fn reiniciar(&mut self) {
        *self = Self::new();
    }

    /// Si los `EnUso` seguidos ya pasaron a la espera larga (el ciclo registra menos).
    pub fn en_uso_espaciado(&self) -> bool {
        self.en_uso_seguidos > EN_USO_ANTES_DE_ESPACIAR
    }

    /// La espera para el próximo intento; con `Red` y `Otra`, avanza su escalón.
    pub fn proxima(&mut self, causa: Causa) -> Espera {
        let (escalon, tope) = match causa {
            Causa::EnUso => {
                self.en_uso_seguidos = self.en_uso_seguidos.saturating_add(1);
                let dura = if self.en_uso_espaciado() { ESPERA_EN_USO_ESPACIADA } else { ESPERA_EN_USO };
                return Espera { dura, despertar: false };
            }
            Causa::Red => (&mut self.red, ESPERA_MAXIMA),
            Causa::Otra => (&mut self.otra, ESPERA_MAXIMA_OTRA),
        };
        let dura = (*escalon).min(tope);
        *escalon = (*escalon * 2).min(tope);
        self.en_uso_seguidos = 0;
        Espera { dura, despertar: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn esperas(r: &mut Reintentos, causa: Causa, n: usize) -> Vec<u64> {
        (0..n).map(|_| r.proxima(causa).dura.as_secs()).collect()
    }

    #[test]
    fn red_crece_1_2_4_y_se_queda_en_5_s() {
        let mut r = Reintentos::new();
        assert_eq!(esperas(&mut r, Causa::Red, 8), vec![1, 2, 4, 5, 5, 5, 5, 5]);
        assert!(r.proxima(Causa::Red).despertar, "con Red se despierta antes de cada intento");
        assert_eq!(ESPERA_MAXIMA, Duration::from_secs(5));
    }

    #[test]
    fn en_uso_espera_3_s_fijos_sin_despertar() {
        let mut r = Reintentos::new();
        for _ in 0..10 {
            assert_eq!(r.proxima(Causa::EnUso), Espera { dura: Duration::from_secs(3), despertar: false });
        }
        assert!(ESPERA_EN_USO <= ESPERA_MAXIMA);
    }

    #[test]
    fn en_uso_no_mueve_el_backoff() {
        let mut r = Reintentos::new();
        assert_eq!(esperas(&mut r, Causa::Red, 2), vec![1, 2]);
        esperas(&mut r, Causa::EnUso, 20);
        assert_eq!(esperas(&mut r, Causa::Red, 2), vec![4, 5], "sigue donde estaba");
    }

    #[test]
    fn otra_crece_hasta_30_s_para_no_martillar() {
        let mut r = Reintentos::new();
        assert_eq!(esperas(&mut r, Causa::Otra, 8), vec![1, 2, 4, 8, 16, 30, 30, 30]);
        assert!(r.proxima(Causa::Otra).despertar);
    }

    #[test]
    fn cada_causa_tiene_su_escalon() {
        let mut r = Reintentos::new();
        esperas(&mut r, Causa::Otra, 7);
        assert_eq!(esperas(&mut r, Causa::Red, 3), vec![1, 2, 4], "la red no hereda el escalón de Otra");
        assert_eq!(esperas(&mut r, Causa::Otra, 1), vec![30], "y la otra sigue en su tope");
    }

    #[test]
    fn despues_de_muchas_red_otra_arranca_en_1_s() {
        let mut r = Reintentos::new();
        esperas(&mut r, Causa::Red, 20);
        assert_eq!(esperas(&mut r, Causa::Otra, 3), vec![1, 2, 4], "un Otra no espera 30 s de entrada");
        assert_eq!(esperas(&mut r, Causa::Red, 1), vec![5], "y la red sigue en su tope");
    }

    #[test]
    fn en_uso_seguido_se_espacia_despues_de_2_minutos() {
        let mut r = Reintentos::new();
        let t = esperas(&mut r, Causa::EnUso, EN_USO_ANTES_DE_ESPACIAR as usize);
        assert!(t.iter().all(|&s| s == 3), "{t:?}");
        assert!(EN_USO_ANTES_DE_ESPACIAR as u64 * ESPERA_EN_USO.as_secs() >= 120, "al menos 2 min de 3 s");
        assert!(!r.en_uso_espaciado());
        for _ in 0..50 {
            assert_eq!(r.proxima(Causa::EnUso), Espera { dura: ESPERA_EN_USO_ESPACIADA, despertar: false });
        }
        assert!(r.en_uso_espaciado());
        assert_eq!(ESPERA_EN_USO_ESPACIADA, Duration::from_secs(15));
    }

    #[test]
    fn otra_causa_o_reiniciar_cortan_la_racha_de_en_uso() {
        let mut r = Reintentos::new();
        esperas(&mut r, Causa::EnUso, 60);
        assert!(r.en_uso_espaciado());
        esperas(&mut r, Causa::Red, 1);
        assert!(!r.en_uso_espaciado());
        assert_eq!(esperas(&mut r, Causa::EnUso, 1), vec![3], "la racha vuelve a empezar");
        esperas(&mut r, Causa::EnUso, 60);
        r.reiniciar();
        assert_eq!(esperas(&mut r, Causa::EnUso, 1), vec![3]);
    }

    #[test]
    fn reiniciar_vuelve_a_1_s() {
        let mut r = Reintentos::new();
        esperas(&mut r, Causa::Otra, 7);
        r.reiniciar();
        assert_eq!(esperas(&mut r, Causa::Red, 4), vec![1, 2, 4, 5]);
        r.reiniciar();
        assert_eq!(esperas(&mut r, Causa::Otra, 1), vec![1]);
        assert_eq!(Reintentos::default().proxima(Causa::Red).dura, ESPERA_INICIAL);
    }
}

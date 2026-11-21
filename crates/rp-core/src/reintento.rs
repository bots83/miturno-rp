//! Cuánto esperar entre intentos de conexión con la PS5, según por qué falló el anterior.
//!
//! - `Red` (corte de red, PS5 que no responde o sin video): 1, 2, 4 y después 5 s fijos. Se arregla
//!   sola y cada segundo de espera es cartel al aire, así que el tope es corto.
//! - `EnUso` ("Remote is already in use"): 3 s fijos y sin despertar. La PS5 está despierta y todavía
//!   tiene la sesión anterior; la rechaza en el pedido HTTP inicial (antes de abrir el control o el
//!   video), así que reintentar seguido no le cuesta nada (el cliente de chiaki-ng reintenta cada pocos ms).
//! - `Otra` (lo que no se arregla solo: versión, PIN, Remote Play caído en la consola, el hilo de
//!   libchiaki muerto): 1, 2, 4, 8, 16 y después 30 s, para no martillar algo que necesita una persona.

use std::time::Duration;

pub const ESPERA_INICIAL: Duration = Duration::from_secs(1);
/// Tope con `Red`.
pub const ESPERA_MAXIMA: Duration = Duration::from_secs(5);
/// Fija con `EnUso`.
pub const ESPERA_EN_USO: Duration = Duration::from_secs(3);
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

/// El backoff: un solo escalón que crece con `Red` y con `Otra` (cada una con su tope). `EnUso` no lo mueve.
#[derive(Debug, Clone)]
pub struct Reintentos {
    siguiente: Duration,
}

impl Default for Reintentos {
    fn default() -> Self {
        Self::new()
    }
}

impl Reintentos {
    pub fn new() -> Self {
        Self { siguiente: ESPERA_INICIAL }
    }

    /// Vuelve la espera a 1 s (sesión estable o reintento pedido a mano).
    pub fn reiniciar(&mut self) {
        self.siguiente = ESPERA_INICIAL;
    }

    /// La espera para el próximo intento; con `Red` y `Otra`, avanza el escalón.
    pub fn proxima(&mut self, causa: Causa) -> Espera {
        let tope = match causa {
            Causa::EnUso => return Espera { dura: ESPERA_EN_USO, despertar: false },
            Causa::Red => ESPERA_MAXIMA,
            Causa::Otra => ESPERA_MAXIMA_OTRA,
        };
        let dura = self.siguiente.min(tope);
        self.siguiente = (self.siguiente * 2).min(ESPERA_MAXIMA_OTRA);
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
    fn despues_de_varias_otra_la_red_igual_tiene_tope_de_5_s() {
        let mut r = Reintentos::new();
        esperas(&mut r, Causa::Otra, 7);
        assert_eq!(esperas(&mut r, Causa::Red, 2), vec![5, 5]);
        assert_eq!(esperas(&mut r, Causa::Otra, 1), vec![30], "y la otra sigue en su tope");
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

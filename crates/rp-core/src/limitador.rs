//! Limita los pedidos de keyframe para que nadie sature el video.

#[derive(Debug, Clone)]
pub struct LimitadorKeyframe {
    min_ms: u64,
    ultimo_permitido: Option<u64>,
}

impl LimitadorKeyframe {
    pub fn new(min_ms: u64) -> Self {
        Self { min_ms, ultimo_permitido: None }
    }

    /// El primero siempre pasa; después, solo si pasaron `min_ms`. Un rechazo no mueve la ventana.
    pub fn permitir(&mut self, ahora_ms: u64) -> bool {
        let ok = match self.ultimo_permitido {
            None => true,
            Some(t) => ahora_ms.saturating_sub(t) >= self.min_ms,
        };
        if ok {
            self.ultimo_permitido = Some(ahora_ms);
        }
        ok
    }

    /// Desde qué momento (ms) un pedido pasaría: lo usa quien difiere un pedido rechazado al final
    /// de la ventana, en vez de perderlo.
    pub fn libre_desde(&self) -> u64 {
        self.ultimo_permitido.map_or(0, |t| t.saturating_add(self.min_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respeta_el_minimo_entre_pedidos() {
        let mut l = LimitadorKeyframe::new(3_000);
        assert!(l.permitir(0));
        assert!(!l.permitir(2_999));
        assert!(l.permitir(3_000));
    }

    #[test]
    fn el_primero_pasa_aunque_el_reloj_sea_chico() {
        assert!(LimitadorKeyframe::new(3_000).permitir(5));
    }

    #[test]
    fn dice_desde_cuando_hay_lugar() {
        let mut l = LimitadorKeyframe::new(3_000);
        assert_eq!(l.libre_desde(), 0, "sin pedidos, hay lugar ya");
        assert!(l.permitir(1_000));
        assert_eq!(l.libre_desde(), 4_000);
        assert!(!l.permitir(2_000));
        assert_eq!(l.libre_desde(), 4_000, "un rechazo no la mueve");
    }

    #[test]
    fn un_rechazo_no_corre_la_ventana() {
        let mut l = LimitadorKeyframe::new(3_000);
        assert!(l.permitir(0));
        assert!(!l.permitir(2_000));
        assert!(l.permitir(3_000));
        assert!(!l.permitir(5_999));
    }
}

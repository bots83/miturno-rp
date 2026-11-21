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
    fn un_rechazo_no_corre_la_ventana() {
        let mut l = LimitadorKeyframe::new(3_000);
        assert!(l.permitir(0));
        assert!(!l.permitir(2_000));
        assert!(l.permitir(3_000));
        assert!(!l.permitir(5_999));
    }
}

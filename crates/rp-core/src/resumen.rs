//! Resumen cuantizado del mando del jugador (lo que sale por `GET /control/mando`).
//!
//! Es lo que el público ve en la tarjeta: los botones tal cual, los gatillos en 16 pasos y los
//! sticks en 17 (de -8 a 8). Con tan pocos valores, un stick que tiembla no inunda el canal.
//! Y positiva hacia abajo, igual que en `/ctl`.

use serde::Serialize;

use crate::mando::EstadoMando;

#[derive(Serialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct ResumenMando {
    /// El turno del jugador que controla; `None` si controla el operador, una vista o nadie.
    #[serde(rename = "turnId")]
    pub turn_id: Option<String>,
    pub b: u16,
    pub l2: u8,
    pub r2: u8,
    pub lx: i8,
    pub ly: i8,
    pub rx: i8,
    pub ry: i8,
}

/// Gatillo de 0 a 255 en 0 a 15.
fn gatillo(v: u8) -> u8 {
    v >> 4
}

/// Eje de -32768 a 32767 en -8 a 8: `v / 4096` redondeado con la mitad lejos del cero.
fn eje(v: i16) -> i8 {
    let v = i32::from(v);
    let paso = (v.abs() + 2048) / 4096;
    (if v < 0 { -paso } else { paso }).clamp(-8, 8) as i8
}

/// Cuantiza el estado que se manda a la PS5. Sin `turn_id` (no controla un jugador), todo en cero.
pub fn resumir(turn_id: Option<&str>, e: &EstadoMando) -> ResumenMando {
    let Some(t) = turn_id else {
        return ResumenMando::default();
    };
    ResumenMando {
        turn_id: Some(t.to_string()),
        b: e.botones as u16,
        l2: gatillo(e.l2),
        r2: gatillo(e.r2),
        lx: eje(e.lx),
        ly: eje(e.ly),
        rx: eje(e.rx),
        ry: eje(e.ry),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mando::{CRUZ, DERECHA, L1, PANEL};

    fn estado(l2: u8, lx: i16) -> EstadoMando {
        EstadoMando { l2, lx, ..EstadoMando::suelto() }
    }

    #[test]
    fn cuantiza_gatillos_y_ejes() {
        let r = |l2, lx| resumir(Some("t1"), &estado(l2, lx));
        assert_eq!(r(255, 0).l2, 15);
        assert_eq!(r(15, 0).l2, 0);
        assert_eq!(r(16, 0).l2, 1);
        assert_eq!(r(0, -32768).lx, -8);
        assert_eq!(r(0, 32767).lx, 8);
        assert_eq!(r(0, 2047).lx, 0);
        assert_eq!(r(0, 2048).lx, 1);
        assert_eq!(r(0, -2048).lx, -1);
        // Mitad lejos del cero, con la fórmula exacta: -6144 / 4096 = -1,5 → -2.
        assert_eq!(r(0, -6144).lx, ((-6144f32 / 4096.0).round()) as i8);
        assert_eq!(r(0, -6144).lx, -2);
        // Los cuatro ejes y el gatillo derecho van cada uno a su campo.
        let e = EstadoMando { r2: 255, ly: 32767, rx: -32768, ry: 4096, ..EstadoMando::suelto() };
        let c = resumir(Some("t1"), &e);
        assert_eq!((c.r2, c.ly, c.rx, c.ry), (15, 8, -8, 1));
    }

    #[test]
    fn sin_turno_todo_cero() {
        let e = EstadoMando { botones: CRUZ, l2: 255, r2: 255, lx: 30000, ly: -30000, rx: 9000, ry: -9000 };
        let r = resumir(None, &e);
        assert_eq!(r, ResumenMando::default());
        assert_eq!(r.turn_id, None);
    }

    #[test]
    fn conserva_bits() {
        let botones = CRUZ | L1 | PANEL | DERECHA;
        let e = EstadoMando { botones, ..EstadoMando::suelto() };
        let r = resumir(Some("t1"), &e);
        assert_eq!(u32::from(r.b), botones);
        assert_eq!(r.turn_id.as_deref(), Some("t1"));
    }

    #[test]
    fn serializa_con_turn_id_en_camel() {
        let r = resumir(Some("t1"), &estado(255, 32767));
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"turnId":"t1","b":0,"l2":15,"r2":0,"lx":8,"ly":0,"rx":0,"ry":0}"#
        );
        assert_eq!(
            serde_json::to_string(&ResumenMando::default()).unwrap(),
            r#"{"turnId":null,"b":0,"l2":0,"r2":0,"lx":0,"ly":0,"rx":0,"ry":0}"#
        );
    }
}

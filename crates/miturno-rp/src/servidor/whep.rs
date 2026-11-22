//! Registro de las sesiones WHEP (WebRTC) de `ps5` que autorizó `/interno/auth`, para expulsar las
//! que dejan de corresponder: MediaMTX no vuelve a validar una sesión abierta (Review Focus 4).
//!
//! - Cada sesión se anota con el `id` que manda MediaMTX en el pedido de autenticación, que es el
//!   mismo UUID de `/v3/webrtcsessions/list` y el que acepta `/v3/webrtcsessions/kick/<id>`.
//! - Una cuenta (salvo el operador) tiene una sola sesión: al registrar una nueva, `registrar`
//!   devuelve las anteriores de esa cuenta para expulsarlas (otra pestaña, o la vista que pasó a jugar
//!   con su credencial nueva).
//! - Al cambiar el turno vigente, `no_autorizadas` devuelve las que `Autorizados::video` ya no deja ver.
//! - Una sesión sale del registro con `quitar`, después de expulsarla (o de que la API diga que ya no
//!   existe). Si la expulsión falla, queda y se reintenta con el próximo empuje.
//! - Las del operador no se registran: nunca se expulsan.
//! - Solo se registran `id` con forma de UUID en minúsculas: nada raro llega a `curl`.
//! - Como mucho `MAX_SESIONES`: con una más se olvida la más vieja.

use std::collections::VecDeque;

use rp_core::{Autorizados, Rol};

/// Tope de sesiones registradas.
pub const MAX_SESIONES: usize = 64;

/// ¿Tiene la forma `^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$`?
pub(crate) fn es_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => matches!(c, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// Una sesión de un jugador o una vista: lo que pide `Autorizados::video`.
struct Sesion {
    id: String,
    rol: Rol,
    sub: String,
    turn_id: Option<String>,
}

/// Las sesiones, de la más vieja a la más nueva.
#[derive(Default)]
pub(super) struct SesionesWhep {
    sesiones: VecDeque<Sesion>,
}

impl SesionesWhep {
    /// Anota la sesión y devuelve los `id` de las otras de la misma cuenta, para expulsarlas. Para el
    /// operador o un `id` que no es UUID, no hace nada.
    pub(super) fn registrar(&mut self, id: &str, rol: Rol, sub: &str, turn_id: Option<&str>) -> Vec<String> {
        if rol == Rol::Operador || !es_uuid(id) {
            return Vec::new();
        }
        self.quitar(id);
        let anteriores = self.sesiones.iter().filter(|s| s.sub == sub).map(|s| s.id.clone()).collect();
        self.sesiones.push_back(Sesion {
            id: id.to_string(),
            rol,
            sub: sub.to_string(),
            turn_id: turn_id.map(String::from),
        });
        while self.sesiones.len() > MAX_SESIONES {
            self.sesiones.pop_front();
        }
        anteriores
    }

    /// Las sesiones registradas que estos autorizados ya no dejan ver (siguen registradas hasta `quitar`).
    pub(super) fn no_autorizadas(&self, a: &Autorizados) -> Vec<String> {
        self.sesiones.iter().filter(|s| !a.video(s.rol, &s.sub, s.turn_id.as_deref())).map(|s| s.id.clone()).collect()
    }

    pub(super) fn quitar(&mut self, id: &str) {
        self.sesiones.retain(|s| s.id != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid(n: usize) -> String {
        format!("{n:08x}-0000-4000-8000-{n:012x}")
    }

    #[test]
    fn la_forma_de_uuid() {
        assert!(es_uuid("0f8e7d6c-5b4a-4321-8fed-cba987654321"));
        assert!(es_uuid(&uuid(1)));
        for raro in [
            "",
            "0F8E7D6C-5B4A-4321-8FED-CBA987654321",
            "0f8e7d6c-5b4a-4321-8fed-cba98765432",
            "0f8e7d6c-5b4a-4321-8fed-cba9876543210",
            "0f8e7d6c5b4a-4321-8fed-cba987654321-",
            "0f8e7d6c-5b4a-4321-8fed-cba98765432g",
            "0f8e7d6c-5b4a-4321-8fed+cba987654321",
            "0f8e7d6c-5b4a-4321-8fed-cba98765432/",
            "../../../../../../../../../../../ab",
            "0f8e7d6c-5b4a-4321-8fed-cba98765432\u{e9}",
        ] {
            assert!(!es_uuid(raro), "{raro:?}");
        }
    }

    #[test]
    fn el_operador_y_los_id_raros_no_se_registran() {
        let mut s = SesionesWhep::default();
        assert!(s.registrar(&uuid(1), Rol::Operador, "op", None).is_empty());
        assert!(s.registrar("no-es-uuid", Rol::Vista, "u2", None).is_empty());
        assert!(s.no_autorizadas(&Autorizados::vacio()).is_empty());
    }

    #[test]
    fn una_sesion_por_cuenta_y_quitar() {
        let mut s = SesionesWhep::default();
        assert!(s.registrar(&uuid(1), Rol::Vista, "u2", None).is_empty());
        assert!(s.registrar(&uuid(2), Rol::Vista, "u3", None).is_empty(), "otra cuenta no expulsa");
        assert_eq!(s.registrar(&uuid(3), Rol::Jugador, "u2", Some("t1")), vec![uuid(1)], "la misma cuenta, aunque cambie el rol");
        // Hasta que se quita, la anterior sigue registrada (si la expulsión falló, se reintenta).
        assert_eq!(s.registrar(&uuid(4), Rol::Jugador, "u2", Some("t1")), vec![uuid(1), uuid(3)]);
        s.quitar(&uuid(1));
        s.quitar(&uuid(3));
        let mut todas = s.no_autorizadas(&Autorizados::vacio());
        todas.sort();
        assert_eq!(todas, vec![uuid(2), uuid(4)]);
    }

    #[test]
    fn no_autorizadas_sigue_la_regla_del_video() {
        let mut s = SesionesWhep::default();
        s.registrar(&uuid(1), Rol::Jugador, "u1", Some("t1"));
        s.registrar(&uuid(2), Rol::Vista, "u2", None);
        let a = rp_core::leer_autorizados(r#"{"version":2,"jugador":{"sub":"u2","turnId":"t2"},"vistas":["u3"]}"#).unwrap();
        assert_eq!(s.no_autorizadas(&a), vec![uuid(1)], "u2 pasó a jugar: conserva el video");
    }

    #[test]
    fn con_mas_de_64_se_olvida_la_mas_vieja() {
        let mut s = SesionesWhep::default();
        for n in 0..=MAX_SESIONES {
            assert!(s.registrar(&uuid(n), Rol::Vista, &format!("u{n}"), None).is_empty());
        }
        let quedan = s.no_autorizadas(&Autorizados::vacio());
        assert_eq!(quedan.len(), MAX_SESIONES);
        assert!(!quedan.contains(&uuid(0)), "la más vieja se olvidó");
        assert!(quedan.contains(&uuid(MAX_SESIONES)));
    }
}

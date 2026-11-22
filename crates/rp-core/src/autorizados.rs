//! Turno vigente y vistas autorizadas, tal como los publica la api de MiTurno.

use crate::credencial::{Credencial, Rol};
use serde::Deserialize;

/// Máximo de vistas autorizadas a la vez.
pub const MAX_VISTAS: usize = 2;
/// Largo máximo (en bytes) de un `sub` o un `turnId`.
pub const MAX_SUB: usize = 64;

/// El jugador que tiene el turno ahora.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JugadorVigente {
    pub sub: String,
    #[serde(rename = "turnId")]
    pub turn_id: String,
}

/// Quién puede mandar y quién puede ver el video.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Autorizados {
    pub version: u64,
    pub jugador: Option<JugadorVigente>,
    pub vistas: Vec<String>,
}

/// Motivo por el que se rechaza el JSON de autorizados.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorAutorizados {
    Formato,
    Vistas,
    Campo,
}

fn campo_valido(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_SUB
}

/// Lee y valida el JSON exacto `{"version":n,"jugador":{"sub":..,"turnId":..}|null,"vistas":[..]}`.
pub fn leer_autorizados(json: &str) -> Result<Autorizados, ErrorAutorizados> {
    let a: Autorizados = serde_json::from_str(json).map_err(|_| ErrorAutorizados::Formato)?;
    if a.vistas.len() > MAX_VISTAS {
        return Err(ErrorAutorizados::Vistas);
    }
    if a.vistas.iter().any(|v| !campo_valido(v)) {
        return Err(ErrorAutorizados::Campo);
    }
    if let Some(j) = &a.jugador {
        if !campo_valido(&j.sub) || !campo_valido(&j.turn_id) {
            return Err(ErrorAutorizados::Campo);
        }
    }
    Ok(a)
}

impl Autorizados {
    /// Sin nadie autorizado (versión 0).
    pub fn vacio() -> Self {
        Self { version: 0, jugador: None, vistas: Vec::new() }
    }

    fn es_vigente(&self, sub: &str, turn_id: Option<&str>) -> bool {
        match (&self.jugador, turn_id) {
            (Some(j), Some(t)) => j.sub == sub && j.turn_id == t,
            _ => false,
        }
    }

    /// ¿Puede esta credencial mandar al mando? Códigos: `TURNO` (no es el turno vigente) y `ROL`.
    pub fn mandos(&self, c: &Credencial) -> Result<(), &'static str> {
        match c.rol {
            Rol::Operador => Ok(()),
            Rol::Jugador if self.es_vigente(&c.sub, c.turn_id.as_deref()) => Ok(()),
            Rol::Jugador => Err("TURNO"),
            Rol::Vista => Err("ROL"),
        }
    }

    /// ¿Puede recibir video? La vista que pasó a jugar (misma cuenta) no lo pierde.
    pub fn video(&self, rol: Rol, sub: &str, turn_id: Option<&str>) -> bool {
        rol == Rol::Operador
            || self.es_vigente(sub, turn_id)
            || self.vistas.iter().any(|v| v == sub)
            || self.jugador.as_ref().is_some_and(|j| j.sub == sub)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::credencial::{Credencial, Rol};

    fn cred(rol: Rol, sub: &str, turn: Option<&str>) -> Credencial {
        Credencial { sub: sub.into(), rol, turn_id: turn.map(String::from), exp: 0, jti: "j".into() }
    }
    fn vigente(sub: &str, turn: &str, vistas: &[&str]) -> Autorizados {
        Autorizados {
            version: 1,
            jugador: Some(JugadorVigente { sub: sub.into(), turn_id: turn.into() }),
            vistas: vistas.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn lee_el_json_con_jugador_nulo_y_sin_vistas() {
        let a = leer_autorizados(r#"{"version":7,"jugador":null,"vistas":[]}"#).unwrap();
        assert_eq!(a, Autorizados { version: 7, jugador: None, vistas: vec![] });
    }

    #[test]
    fn lee_jugador_y_vistas() {
        let a = leer_autorizados(r#"{"version":1,"jugador":{"sub":"u1","turnId":"t1"},"vistas":["u2","u3"]}"#).unwrap();
        assert_eq!(a, vigente("u1", "t1", &["u2", "u3"]));
    }

    #[test]
    fn rechaza_tres_vistas_campos_extra_y_vacios_o_largos() {
        assert!(leer_autorizados(r#"{"version":1,"jugador":null,"vistas":["a","b","c"]}"#).is_err());
        assert!(leer_autorizados(r#"{"version":1,"jugador":null,"vistas":[],"extra":1}"#).is_err());
        assert!(leer_autorizados(r#"{"version":1,"jugador":{"sub":"","turnId":"t"},"vistas":[]}"#).is_err());
        assert!(leer_autorizados(r#"{"version":1,"jugador":{"sub":"u","turnId":""},"vistas":[]}"#).is_err());
        let largo = "x".repeat(65);
        let j = format!(r#"{{"version":1,"jugador":{{"sub":"{largo}","turnId":"t"}},"vistas":[]}}"#);
        assert!(leer_autorizados(&j).is_err());
        let v = format!(r#"{{"version":1,"jugador":null,"vistas":["{largo}"]}}"#);
        assert!(leer_autorizados(&v).is_err());
        let justo = "x".repeat(64);
        let ok = format!(r#"{{"version":1,"jugador":{{"sub":"{justo}","turnId":"t"}},"vistas":[]}}"#);
        assert!(leer_autorizados(&ok).is_ok());
        assert!(leer_autorizados(r#"{"version":1,"jugador":{"sub":"u","turnId":"t","x":1},"vistas":[]}"#).is_err());
    }

    #[test]
    fn mandos_segun_rol_y_turno() {
        let v = vigente("u1", "t1", &[]);
        assert_eq!(Autorizados::vacio().mandos(&cred(Rol::Operador, "op", None)), Ok(()));
        assert_eq!(v.mandos(&cred(Rol::Jugador, "u1", Some("t1"))), Ok(()));
        assert_eq!(v.mandos(&cred(Rol::Jugador, "u1", Some("t0"))), Err("TURNO"));
        assert_eq!(v.mandos(&cred(Rol::Jugador, "u2", Some("t1"))), Err("TURNO"));
        assert_eq!(Autorizados::vacio().mandos(&cred(Rol::Jugador, "u1", Some("t1"))), Err("TURNO"));
        assert_eq!(v.mandos(&cred(Rol::Vista, "u1", None)), Err("ROL"));
    }

    #[test]
    fn video_segun_rol_vistas_y_cuenta() {
        assert!(Autorizados::vacio().video(Rol::Operador, "op", None));
        let con = Autorizados { version: 1, jugador: None, vistas: vec!["u3".into()] };
        assert!(con.video(Rol::Vista, "u3", None));
        assert!(!Autorizados::vacio().video(Rol::Vista, "u3", None));
        let v = vigente("u1", "t1", &[]);
        assert!(v.video(Rol::Jugador, "u1", Some("t0")));
        assert!(v.video(Rol::Jugador, "u1", Some("t1")));
        assert!(!v.video(Rol::Jugador, "u9", Some("t0")));
    }
}

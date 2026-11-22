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
    /// La clave es obligatoria (`null` cuando no hay jugador): con `deserialize_with`, serde deja de
    /// tomar un `Option` ausente como `None`.
    #[serde(deserialize_with = "Option::deserialize")]
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
        self.mandos_de(c.rol, &c.sub, c.turn_id.as_deref())
    }

    /// La regla de `mandos` con lo único que mira de la credencial: rol, `sub` y `turnId` (así la usa
    /// también quien ya no tiene la credencial, como el que controla al llegar un turno nuevo).
    pub fn mandos_de(&self, rol: Rol, sub: &str, turn_id: Option<&str>) -> Result<(), &'static str> {
        match rol {
            Rol::Operador => Ok(()),
            Rol::Jugador if self.es_vigente(sub, turn_id) => Ok(()),
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
        use ErrorAutorizados::{Campo, Formato, Vistas};
        let err = |j: &str| leer_autorizados(j).err();
        assert_eq!(err(r#"{"version":1,"jugador":null,"vistas":["a","b","c"]}"#), Some(Vistas));
        assert_eq!(err(r#"{"version":1,"jugador":null,"vistas":[],"extra":1}"#), Some(Formato));
        assert_eq!(err(r#"{"version":1,"jugador":{"sub":"","turnId":"t"},"vistas":[]}"#), Some(Campo));
        assert_eq!(err(r#"{"version":1,"jugador":{"sub":"u","turnId":""},"vistas":[]}"#), Some(Campo));
        let largo = "x".repeat(65);
        let j = format!(r#"{{"version":1,"jugador":{{"sub":"{largo}","turnId":"t"}},"vistas":[]}}"#);
        assert_eq!(err(&j), Some(Campo));
        let j = format!(r#"{{"version":1,"jugador":{{"sub":"u","turnId":"{largo}"}},"vistas":[]}}"#);
        assert_eq!(err(&j), Some(Campo));
        let v = format!(r#"{{"version":1,"jugador":null,"vistas":["{largo}"]}}"#);
        assert_eq!(err(&v), Some(Campo));
        let justo = "x".repeat(64);
        let ok = format!(r#"{{"version":1,"jugador":{{"sub":"{justo}","turnId":"{justo}"}},"vistas":["{justo}"]}}"#);
        assert!(leer_autorizados(&ok).is_ok());
        assert_eq!(err(r#"{"version":1,"jugador":{"sub":"u","turnId":"t","x":1},"vistas":[]}"#), Some(Formato));
    }

    #[test]
    fn la_clave_jugador_es_obligatoria_aunque_sea_null() {
        assert_eq!(leer_autorizados(r#"{"version":1,"vistas":[]}"#).err(), Some(ErrorAutorizados::Formato));
        assert!(leer_autorizados(r#"{"version":1,"jugador":null,"vistas":[]}"#).is_ok());
    }

    #[test]
    fn rechaza_json_invalido_y_claves_faltantes_o_de_otro_tipo() {
        use ErrorAutorizados::{Campo, Formato};
        let err = |j: &str| leer_autorizados(j).err();
        for j in [
            "",
            "no es json",
            "[]",
            "null",
            r#"{"jugador":null,"vistas":[]}"#,
            r#"{"version":1,"jugador":null}"#,
            r#"{"version":-1,"jugador":null,"vistas":[]}"#,
            r#"{"version":"1","jugador":null,"vistas":[]}"#,
            r#"{"version":1,"jugador":{"sub":"u"},"vistas":[]}"#,
            r#"{"version":1,"jugador":{"sub":"u","turnId":null},"vistas":[]}"#,
            r#"{"version":1,"jugador":{"turnId":"t"},"vistas":[]}"#,
            r#"{"version":1,"jugador":"u1","vistas":[]}"#,
            r#"{"version":1,"jugador":null,"vistas":[1]}"#,
        ] {
            assert_eq!(err(j), Some(Formato), "{j:?}");
        }
        assert_eq!(err(r#"{"version":1,"jugador":null,"vistas":[""]}"#), Some(Campo), "vista con sub vacío");
        assert_eq!(err(r#"{"version":1,"jugador":null,"vistas":["u2",""]}"#), Some(Campo));
    }

    #[test]
    fn un_jugador_sin_turn_id_en_la_credencial_no_tiene_mandos_ni_es_el_vigente() {
        let v = vigente("u1", "t1", &[]);
        assert_eq!(v.mandos(&cred(Rol::Jugador, "u1", None)), Err("TURNO"));
        // El video lo conserva la misma cuenta (la vista que pasó a jugar), con o sin turnId.
        assert!(v.video(Rol::Jugador, "u1", None));
        assert!(!v.video(Rol::Jugador, "u2", None));
        assert!(!v.video(Rol::Vista, "u2", None));
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
    fn mandos_de_es_la_misma_regla_que_mandos_sin_armar_una_credencial() {
        let v = vigente("u1", "t1", &["u2"]);
        assert_eq!(v.mandos_de(Rol::Operador, "op", None), Ok(()));
        assert_eq!(Autorizados::vacio().mandos_de(Rol::Operador, "op", None), Ok(()));
        assert_eq!(v.mandos_de(Rol::Jugador, "u1", Some("t1")), Ok(()));
        assert_eq!(v.mandos_de(Rol::Jugador, "u1", Some("t0")), Err("TURNO"));
        assert_eq!(v.mandos_de(Rol::Jugador, "u1", None), Err("TURNO"));
        assert_eq!(v.mandos_de(Rol::Jugador, "u2", Some("t1")), Err("TURNO"));
        assert_eq!(v.mandos_de(Rol::Vista, "u2", None), Err("ROL"), "una vista nunca manda");
        assert_eq!(v.mandos_de(Rol::Vista, "u1", Some("t1")), Err("ROL"));
        for (rol, sub, turno) in
            [(Rol::Jugador, "u1", Some("t1")), (Rol::Jugador, "u1", Some("t0")), (Rol::Vista, "u1", None)]
        {
            assert_eq!(v.mandos(&cred(rol, sub, turno)), v.mandos_de(rol, sub, turno), "{rol:?} {sub} {turno:?}");
        }
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

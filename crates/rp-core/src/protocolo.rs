//! Mensajes JSON de `/ctl`. El mismo formato lo habla la web (`packages/controls`).
use crate::credencial::Rol;
use serde::{Deserialize, Serialize};

/// Lo que manda el cliente (web u operador).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase", deny_unknown_fields)]
pub enum MsgCliente {
    Auth { token: String },
    Estado { botones: u32, l2: u8, r2: u8, lx: i16, ly: i16, rx: i16, ry: i16 },
    Latido,
    #[serde(rename = "keyframe")]
    PedirKeyframe,
}

/// Lo que responde el servidor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum MsgServidor {
    Ok { rol: Rol },
    Error { codigo: String },
    Reemplazado,
}

impl MsgServidor {
    pub fn error(codigo: &str) -> Self {
        Self::Error { codigo: codigo.to_string() }
    }

    pub fn a_json(&self) -> String {
        serde_json::to_string(self).expect("un mensaje del servidor siempre se serializa")
    }
}

/// Lee un mensaje del cliente. Campos o tipos desconocidos, y números fuera de rango, son error.
pub fn leer_mensaje(texto: &str) -> Result<MsgCliente, serde_json::Error> {
    serde_json::from_str(texto)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credencial::Rol;

    #[test]
    fn auth_ida_y_vuelta() {
        let m = leer_mensaje(r#"{"t":"auth","token":"abc"}"#).unwrap();
        assert_eq!(m, MsgCliente::Auth { token: "abc".into() });
    }

    #[test]
    fn estado_ida_y_vuelta() {
        let j = r#"{"t":"estado","botones":1025,"l2":255,"r2":0,"lx":-32768,"ly":32767,"rx":0,"ry":-1}"#;
        let m = leer_mensaje(j).unwrap();
        assert_eq!(
            m,
            MsgCliente::Estado { botones: 1025, l2: 255, r2: 0, lx: -32768, ly: 32767, rx: 0, ry: -1 }
        );
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(v, serde_json::from_str::<serde_json::Value>(j).unwrap());
    }

    #[test]
    fn latido_y_keyframe() {
        assert_eq!(leer_mensaje(r#"{"t":"latido"}"#).unwrap(), MsgCliente::Latido);
        assert_eq!(leer_mensaje(r#"{"t":"keyframe"}"#).unwrap(), MsgCliente::PedirKeyframe);
        assert_eq!(serde_json::to_string(&MsgCliente::Latido).unwrap(), r#"{"t":"latido"}"#);
        assert_eq!(serde_json::to_string(&MsgCliente::PedirKeyframe).unwrap(), r#"{"t":"keyframe"}"#);
    }

    #[test]
    fn el_servidor_serializa_exacto() {
        assert_eq!(MsgServidor::Ok { rol: Rol::Operador }.a_json(), r#"{"t":"ok","rol":"operador"}"#);
        assert_eq!(MsgServidor::Ok { rol: Rol::Jugador }.a_json(), r#"{"t":"ok","rol":"jugador"}"#);
        assert_eq!(MsgServidor::Ok { rol: Rol::Vista }.a_json(), r#"{"t":"ok","rol":"vista"}"#);
        assert_eq!(MsgServidor::error("VENCIDA").a_json(), r#"{"t":"error","codigo":"VENCIDA"}"#);
        assert_eq!(MsgServidor::Reemplazado.a_json(), r#"{"t":"reemplazado"}"#);
    }

    #[test]
    fn el_servidor_vuelve_a_leerse() {
        for m in [
            MsgServidor::Ok { rol: Rol::Vista },
            MsgServidor::error("FORMATO"),
            MsgServidor::Reemplazado,
        ] {
            assert_eq!(serde_json::from_str::<MsgServidor>(&m.a_json()).unwrap(), m);
        }
    }

    #[test]
    fn json_desconocido_es_error() {
        assert!(leer_mensaje("no es json").is_err());
        assert!(leer_mensaje(r#"{"t":"explotar"}"#).is_err());
        assert!(leer_mensaje(r#"{"botones":0}"#).is_err());
        assert!(leer_mensaje(r#"{"t":"auth"}"#).is_err());
        assert!(leer_mensaje(r#"{"t":"auth","token":"a","extra":1}"#).is_err());
        assert!(leer_mensaje(r#"{"t":"estado","botones":0,"l2":0,"r2":0,"lx":0,"ly":0,"rx":0,"ry":0,"x":1}"#).is_err());
        assert!(leer_mensaje(r#"{"t":"estado","botones":0,"l2":0}"#).is_err());
    }

    #[test]
    fn valores_fuera_de_rango_son_error() {
        let base = |campo: &str, v: &str| {
            let mut c = vec![("botones", "0"), ("l2", "0"), ("r2", "0"), ("lx", "0"), ("ly", "0"), ("rx", "0"), ("ry", "0")];
            for x in c.iter_mut() {
                if x.0 == campo {
                    x.1 = v;
                }
            }
            let cuerpo: Vec<String> = c.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
            format!("{{\"t\":\"estado\",{}}}", cuerpo.join(","))
        };
        assert!(leer_mensaje(&base("l2", "300")).is_err());
        assert!(leer_mensaje(&base("r2", "-1")).is_err());
        assert!(leer_mensaje(&base("lx", "40000")).is_err());
        assert!(leer_mensaje(&base("ry", "-32769")).is_err());
        assert!(leer_mensaje(&base("botones", "4294967296")).is_err());
        assert!(leer_mensaje(&base("botones", "-1")).is_err());
    }
}

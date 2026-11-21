//! Credencial de turno firmada (JWT EdDSA) que emite la api de MiTurno.

use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

const EMISOR: &str = "miturno";
const AUDIENCIA: &str = "miturno-video";
/// Tolerancia de reloj, en segundos, para el vencimiento.
const TOLERANCIA_S: u64 = 30;

/// Rol de la conexión. El orden importa: `Operador > Jugador > Vista`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rol {
    Vista,
    Jugador,
    Operador,
}

/// Credencial ya validada.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credencial {
    pub sub: String,
    pub rol: Rol,
    pub turn_id: Option<String>,
    pub exp: u64,
    pub jti: String,
}

/// Motivo por el que se rechaza una credencial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCredencial {
    Formato,
    Firma,
    Emisor,
    Audiencia,
    Vencida,
    Rol,
}

impl ErrorCredencial {
    /// Código estable que se devuelve al cliente.
    pub fn codigo(&self) -> &'static str {
        match self {
            Self::Formato => "FORMATO",
            Self::Firma => "FIRMA",
            Self::Emisor => "EMISOR",
            Self::Audiencia => "AUDIENCIA",
            Self::Vencida => "VENCIDA",
            Self::Rol => "ROL",
        }
    }
}

/// `aud` puede venir como texto o como lista.
#[derive(Deserialize)]
#[serde(untagged)]
enum Audiencia {
    Una(String),
    Varias(Vec<String>),
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    aud: Audiencia,
    sub: String,
    rol: String,
    #[serde(rename = "turnId")]
    turn_id: Option<String>,
    exp: u64,
    jti: String,
}

/// Valida firma (solo EdDSA), emisor, audiencia, vencimiento y rol. El tiempo es `ahora_s`, no el reloj
/// del sistema, para que sea determinista. La firma se chequea primero; recién después los claims.
pub fn validar(token: &str, clave_publica_pem: &str, ahora_s: u64) -> Result<Credencial, ErrorCredencial> {
    let clave = DecodingKey::from_ed_pem(clave_publica_pem.as_bytes()).map_err(|_| ErrorCredencial::Firma)?;
    let mut v = Validation::new(Algorithm::EdDSA);
    // Los claims se chequean a mano abajo: así los errores son precisos y no dependen del reloj.
    v.required_spec_claims.clear();
    v.validate_exp = false;
    v.validate_nbf = false;
    v.validate_aud = false;
    let datos = decode::<Claims>(token, &clave, &v).map_err(|e| match e.kind() {
        ErrorKind::InvalidSignature | ErrorKind::InvalidAlgorithm | ErrorKind::InvalidKeyFormat => {
            ErrorCredencial::Firma
        }
        _ => ErrorCredencial::Formato,
    })?;
    let c = datos.claims;
    if c.iss != EMISOR {
        return Err(ErrorCredencial::Emisor);
    }
    let aud_ok = match &c.aud {
        Audiencia::Una(a) => a == AUDIENCIA,
        Audiencia::Varias(l) => l.iter().any(|a| a == AUDIENCIA),
    };
    if !aud_ok {
        return Err(ErrorCredencial::Audiencia);
    }
    if c.exp.saturating_add(TOLERANCIA_S) < ahora_s {
        return Err(ErrorCredencial::Vencida);
    }
    let rol = match c.rol.as_str() {
        "operador" => Rol::Operador,
        "jugador" => Rol::Jugador,
        "vista" => Rol::Vista,
        _ => return Err(ErrorCredencial::Rol),
    };
    Ok(Credencial { sub: c.sub, rol, turn_id: c.turn_id, exp: c.exp, jti: c.jti })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
    use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
    use ed25519_dalek::SigningKey;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde_json::{json, Value};

    const AHORA: u64 = 1_800_000_000;

    /// Par de claves Ed25519 armado en el test (nunca se versiona una clave): (privada PKCS8, pública SPKI).
    fn par(semilla: u8) -> (String, String) {
        let sk = SigningKey::from_bytes(&[semilla; 32]);
        let privada = sk.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
        let publica = sk.verifying_key().to_public_key_pem(LineEnding::LF).unwrap();
        (privada, publica)
    }

    fn claims() -> Value {
        json!({"iss":"miturno","aud":"miturno-video","sub":"u1","rol":"operador",
               "turnId":"t9","iat":AHORA - 10,"exp":AHORA + 60,"jti":"j1"})
    }

    fn firmar(privada: &str, claims: &Value) -> String {
        let k = EncodingKey::from_ed_pem(privada.as_bytes()).unwrap();
        encode(&Header::new(Algorithm::EdDSA), claims, &k).unwrap()
    }

    #[test]
    fn valido_de_operador() {
        let (sk, pk) = par(1);
        let c = validar(&firmar(&sk, &claims()), &pk, AHORA).unwrap();
        assert_eq!(c.rol, Rol::Operador);
        assert_eq!(c.sub, "u1");
        assert_eq!(c.turn_id.as_deref(), Some("t9"));
        assert_eq!(c.exp, AHORA + 60);
        assert_eq!(c.jti, "j1");
    }

    #[test]
    fn turn_id_es_opcional() {
        let (sk, pk) = par(1);
        let mut c = claims();
        c.as_object_mut().unwrap().remove("turnId");
        assert_eq!(validar(&firmar(&sk, &c), &pk, AHORA).unwrap().turn_id, None);
    }

    #[test]
    fn roles_validos_y_orden() {
        let (sk, pk) = par(1);
        for (txt, rol) in [("jugador", Rol::Jugador), ("vista", Rol::Vista)] {
            let mut c = claims();
            c["rol"] = json!(txt);
            assert_eq!(validar(&firmar(&sk, &c), &pk, AHORA).unwrap().rol, rol);
        }
        assert!(Rol::Operador > Rol::Jugador && Rol::Jugador > Rol::Vista);
    }

    #[test]
    fn firmado_con_otra_clave_es_firma() {
        let (sk_otra, _) = par(2);
        let (_, pk) = par(1);
        assert_eq!(validar(&firmar(&sk_otra, &claims()), &pk, AHORA), Err(ErrorCredencial::Firma));
    }

    #[test]
    fn audiencia_distinta() {
        let (sk, pk) = par(1);
        let mut c = claims();
        c["aud"] = json!("otro");
        assert_eq!(validar(&firmar(&sk, &c), &pk, AHORA), Err(ErrorCredencial::Audiencia));
    }

    #[test]
    fn emisor_distinto() {
        let (sk, pk) = par(1);
        let mut c = claims();
        c["iss"] = json!("otro");
        assert_eq!(validar(&firmar(&sk, &c), &pk, AHORA), Err(ErrorCredencial::Emisor));
    }

    #[test]
    fn tolerancia_de_30_segundos() {
        let (sk, pk) = par(1);
        let mut c = claims();
        c["exp"] = json!(AHORA - 31);
        assert_eq!(validar(&firmar(&sk, &c), &pk, AHORA), Err(ErrorCredencial::Vencida));
        c["exp"] = json!(AHORA - 29);
        assert!(validar(&firmar(&sk, &c), &pk, AHORA).is_ok());
    }

    #[test]
    fn rol_desconocido() {
        let (sk, pk) = par(1);
        let mut c = claims();
        c["rol"] = json!("admin");
        assert_eq!(validar(&firmar(&sk, &c), &pk, AHORA), Err(ErrorCredencial::Rol));
    }

    #[test]
    fn hs256_con_la_publica_como_secreto_es_firma() {
        let (_, pk) = par(1);
        let k = EncodingKey::from_secret(pk.as_bytes());
        let t = encode(&Header::new(Algorithm::HS256), &claims(), &k).unwrap();
        assert_eq!(validar(&t, &pk, AHORA), Err(ErrorCredencial::Firma));
    }

    #[test]
    fn basura_es_formato() {
        let (_, pk) = par(1);
        for t in ["", "basura", "a.b.c", "...."] {
            assert_eq!(validar(t, &pk, AHORA), Err(ErrorCredencial::Formato), "token {t:?}");
        }
    }

    #[test]
    fn falta_jti_o_sub_es_formato() {
        let (sk, pk) = par(1);
        for campo in ["jti", "sub"] {
            let mut c = claims();
            c.as_object_mut().unwrap().remove(campo);
            assert_eq!(validar(&firmar(&sk, &c), &pk, AHORA), Err(ErrorCredencial::Formato), "{campo}");
        }
    }

    #[test]
    fn codigos_de_error() {
        use ErrorCredencial::*;
        let v = [(Formato, "FORMATO"), (Firma, "FIRMA"), (Emisor, "EMISOR"),
                 (Audiencia, "AUDIENCIA"), (Vencida, "VENCIDA"), (Rol, "ROL")];
        for (e, c) in v {
            assert_eq!(e.codigo(), c);
        }
    }
}

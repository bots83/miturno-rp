//! Credencial de turno firmada (JWT EdDSA) que emite la api de MiTurno.

use ed25519_dalek_2::pkcs8::DecodePublicKey;
use ed25519_dalek_2::VerifyingKey;
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
    /// Opcional (la api no lo emite): si viene, no puede estar más allá de la tolerancia.
    nbf: Option<u64>,
    jti: String,
}

/// Clave pública Ed25519 ya parseada y verificada. Se arma una sola vez al arrancar: así una clave del
/// servidor rota falla temprano con un error claro, en lugar de parecer un error de firma del cliente.
#[derive(Clone)]
pub struct ClaveVerificacion(DecodingKey);

/// La clave pública del servidor no es una clave Ed25519 en PEM (SPKI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorClave;

impl std::fmt::Display for ErrorClave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no es una clave pública Ed25519 en PEM (\"BEGIN PUBLIC KEY\")")
    }
}

impl std::error::Error for ErrorClave {}

impl ClaveVerificacion {
    /// Parsea la clave completa (PEM, SPKI, OID de Ed25519 y punto válido de la curva).
    pub fn desde_pem(pem: &str) -> Result<Self, ErrorClave> {
        let vk = VerifyingKey::from_public_key_pem(pem.trim()).map_err(|_| ErrorClave)?;
        // jsonwebtoken guarda la clave Ed como los 32 bytes crudos.
        Ok(Self(DecodingKey::from_ed_der(vk.as_bytes())))
    }
}

impl Credencial {
    /// Último segundo (Unix) en que `validar` todavía la acepta: `exp` más la tolerancia de reloj.
    /// Una conexión abierta con esta credencial se corta al llegar a este instante.
    pub fn vence_s(&self) -> u64 {
        self.exp.saturating_add(TOLERANCIA_S)
    }
}

/// Como `validar_con`, parseando la clave en cada llamada (para tests y herramientas). Una clave
/// inválida da `Firma`; el servidor usa `ClaveVerificacion::desde_pem` al arrancar.
pub fn validar(token: &str, clave_publica_pem: &str, ahora_s: u64) -> Result<Credencial, ErrorCredencial> {
    let clave = ClaveVerificacion::desde_pem(clave_publica_pem).map_err(|_| ErrorCredencial::Firma)?;
    validar_con(token, &clave, ahora_s)
}

/// Valida firma (solo EdDSA), emisor, audiencia, vencimiento, `nbf` (si viene) y rol. `iat` se ignora. El tiempo es `ahora_s`, no el reloj
/// del sistema, para que sea determinista. La firma se chequea primero; recién después los claims.
pub fn validar_con(token: &str, clave: &ClaveVerificacion, ahora_s: u64) -> Result<Credencial, ErrorCredencial> {
    let clave = &clave.0;
    let mut v = Validation::new(Algorithm::EdDSA);
    // Los claims se chequean a mano abajo: así los errores son precisos y no dependen del reloj.
    v.required_spec_claims.clear();
    v.validate_exp = false;
    v.validate_nbf = false;
    v.validate_aud = false;
    let datos = decode::<Claims>(token, clave, &v).map_err(|e| match e.kind() {
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
    // Una credencial que todavía no vale no es de este contrato (la api no emite `nbf`).
    if c.nbf.is_some_and(|nbf| nbf > ahora_s.saturating_add(TOLERANCIA_S)) {
        return Err(ErrorCredencial::Formato);
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
    fn falta_cualquier_claim_obligatorio_es_formato() {
        let (sk, pk) = par(1);
        for campo in ["iss", "aud", "exp", "rol", "sub", "jti"] {
            let mut c = claims();
            c.as_object_mut().unwrap().remove(campo);
            assert_eq!(validar(&firmar(&sk, &c), &pk, AHORA), Err(ErrorCredencial::Formato), "{campo}");
        }
    }

    /// base64url sin relleno (como en los JWT), sin sumar una dependencia solo para los tests.
    fn b64(datos: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut s = String::new();
        for t in datos.chunks(3) {
            let n = t.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
            for i in 0..=t.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        s
    }

    #[test]
    fn b64_de_prueba_coincide_con_el_de_los_jwt() {
        let t = firmar(&par(1).0, &claims());
        let cabecera = t.split('.').next().unwrap();
        assert_eq!(b64(br#"{"typ":"JWT","alg":"EdDSA"}"#), cabecera);
    }

    /// Cambia el payload de un token ya firmado (la firma queda la de antes).
    fn con_payload(token: &str, payload: &Value) -> String {
        let partes: Vec<&str> = token.split('.').collect();
        format!("{}.{}.{}", partes[0], b64(payload.to_string().as_bytes()), partes[2])
    }

    #[test]
    fn payload_adulterado_con_la_firma_de_otro_es_firma() {
        let (sk, pk) = par(1);
        let t = firmar(&sk, &claims());
        let mut c = claims();
        c["rol"] = json!("operador");
        c["sub"] = json!("otro");
        assert_eq!(validar(&con_payload(&t, &c), &pk, AHORA), Err(ErrorCredencial::Firma));
    }

    #[test]
    fn alg_none_sin_firma_es_formato() {
        let (_, pk) = par(1);
        let cab = b64(json!({"alg":"none","typ":"JWT"}).to_string().as_bytes());
        let cuerpo = b64(claims().to_string().as_bytes());
        for t in [format!("{cab}.{cuerpo}."), format!("{cab}.{cuerpo}")] {
            assert_eq!(validar(&t, &pk, AHORA), Err(ErrorCredencial::Formato), "{t}");
        }
    }

    #[test]
    fn el_borde_de_la_tolerancia_todavia_vale() {
        let (sk, pk) = par(1);
        let mut c = claims();
        c["exp"] = json!(AHORA - 30);
        assert!(validar(&firmar(&sk, &c), &pk, AHORA).is_ok(), "exp = ahora − 30 todavía vale");
        c["exp"] = json!(AHORA - 30 - 1);
        assert_eq!(validar(&firmar(&sk, &c), &pk, AHORA), Err(ErrorCredencial::Vencida));
    }

    #[test]
    fn nbf_en_el_futuro_mas_alla_de_la_tolerancia_no_vale() {
        let (sk, pk) = par(1);
        let mut c = claims();
        c["nbf"] = json!(AHORA + 30);
        assert!(validar(&firmar(&sk, &c), &pk, AHORA).is_ok(), "nbf dentro de la tolerancia vale");
        c["nbf"] = json!(AHORA + 31);
        assert_eq!(validar(&firmar(&sk, &c), &pk, AHORA), Err(ErrorCredencial::Formato));
        c["nbf"] = json!("mañana");
        assert_eq!(validar(&firmar(&sk, &c), &pk, AHORA), Err(ErrorCredencial::Formato));
    }

    #[test]
    fn validar_con_la_clave_parseada_una_vez() {
        let (sk, pk) = par(1);
        let clave = ClaveVerificacion::desde_pem(&pk).unwrap();
        let t = firmar(&sk, &claims());
        assert_eq!(validar_con(&t, &clave, AHORA), validar(&t, &pk, AHORA));
        assert_eq!(validar_con(&t, &clave, AHORA).unwrap().rol, Rol::Operador);
        let (sk_otra, _) = par(2);
        assert_eq!(validar_con(&firmar(&sk_otra, &claims()), &clave, AHORA), Err(ErrorCredencial::Firma));
    }

    #[test]
    fn clave_del_servidor_invalida_falla_al_parsear_no_como_firma() {
        let (privada, _) = par(1);
        // SPKI de X25519 (OID 1.3.101.110): bien formado, pero no es Ed25519.
        let x25519 = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VuAyEAAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\n-----END PUBLIC KEY-----\n";
        // SPKI de Ed25519 con la clave cortada (31 bytes).
        let corta = "-----BEGIN PUBLIC KEY-----\nMCkwBQYDK2VwAyAAAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ==\n-----END PUBLIC KEY-----\n";
        for (nombre, pem) in [("vacía", ""), ("basura", "no es pem"), ("privada", privada.as_str()), ("x25519", x25519), ("corta", corta)] {
            assert!(ClaveVerificacion::desde_pem(pem).is_err(), "{nombre}");
        }
    }

    #[test]
    fn vence_s_es_el_ultimo_segundo_aceptado() {
        let (sk, pk) = par(1);
        let c = validar(&firmar(&sk, &claims()), &pk, AHORA).unwrap();
        assert_eq!(c.vence_s(), AHORA + 60 + 30);
        let mut cl = claims();
        cl["exp"] = json!(AHORA + 10);
        let t = firmar(&sk, &cl);
        let c = validar(&t, &pk, AHORA).unwrap();
        assert!(validar(&t, &pk, c.vence_s()).is_ok());
        assert_eq!(validar(&t, &pk, c.vence_s() + 1), Err(ErrorCredencial::Vencida));
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

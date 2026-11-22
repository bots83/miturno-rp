//! Registro de las sesiones WHEP (WebRTC) de `ps5` que autorizó `/interno/auth`, para expulsar las
//! que dejan de corresponder: MediaMTX no vuelve a validar una sesión abierta (Review Focus 4).
//!
//! - Cada sesión se anota con el `id` que manda MediaMTX en el pedido de autenticación, que es el
//!   mismo UUID de `/v3/webrtc/sessions/list` y el que acepta `/v3/webrtc/sessions/kick/<id>`.
//! - Una cuenta (salvo el operador) tiene una sola sesión: al registrar una nueva, `registrar`
//!   devuelve las anteriores de esa cuenta para expulsarlas (otra pestaña, o la vista que pasó a jugar
//!   con su credencial nueva).
//! - Al cambiar el turno vigente, `no_autorizadas` devuelve las que `Autorizados::video` ya no deja ver.
//! - Lo que se devuelve para expulsar queda **pendiente** y no se vuelve a devolver mientras la
//!   expulsión está en vuelo (N pestañas a la vez no son N² pedidos). Sale del registro con `quitar`
//!   cuando la expulsión sale bien; si falla del todo, `fallida` la deja lista para el próximo intento.
//! - Las del operador no se registran: nunca se expulsan.
//! - Solo se registran `id` con forma de UUID en minúsculas: nada raro llega a `curl`.
//! - Como mucho `MAX_SESIONES`: con una más, la más vieja sale del registro y se expulsa.
//! - Se guarda en disco (`a_json`, `cargar`; en el `StateDirectory`) para que un reinicio de miturno-rp
//!   no deje sin expulsar los WHEP abiertos: al arrancar se carga y el primer empuje del turno decide.
//!   Lo pendiente no se guarda (al arrancar no hay nada en vuelo).

use std::collections::VecDeque;
use std::path::Path;

use rp_core::{Autorizados, Rol, MAX_SUB};
use serde::{Deserialize, Serialize};

/// Tope de sesiones registradas.
pub const MAX_SESIONES: usize = 64;
/// Tope del archivo del registro (64 sesiones ocupan unos 12 KiB).
const MAX_ARCHIVO: u64 = 64 * 1024;

/// ¿Tiene la forma `^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$`?
pub(crate) fn es_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => matches!(c, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// Una sesión de un jugador o una vista: lo que pide `Autorizados::video`, y si su expulsión está en vuelo.
struct Sesion {
    id: String,
    rol: Rol,
    sub: String,
    turn_id: Option<String>,
    pendiente: bool,
}

/// Lo que se guarda de cada sesión (sin secretos: ni credencial ni token).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SesionGuardada {
    id: String,
    rol: Rol,
    sub: String,
    #[serde(rename = "turnId", deserialize_with = "Option::deserialize")]
    turn_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Guardado {
    sesiones: Vec<SesionGuardada>,
}

/// Las sesiones, de la más vieja a la más nueva.
#[derive(Default)]
pub(super) struct SesionesWhep {
    sesiones: VecDeque<Sesion>,
}

impl SesionesWhep {
    /// Anota la sesión y devuelve, para expulsarlas, las otras de la misma cuenta y la que sale por el
    /// tope (salvo las que ya están pendientes). Para el operador o un `id` que no es UUID, no hace nada.
    pub(super) fn registrar(&mut self, id: &str, rol: Rol, sub: &str, turn_id: Option<&str>) -> Vec<String> {
        if rol == Rol::Operador || !es_uuid(id) {
            return Vec::new();
        }
        self.quitar(id);
        let mut expulsar: Vec<String> = Vec::new();
        for s in self.sesiones.iter_mut().filter(|s| s.sub == sub && !s.pendiente) {
            s.pendiente = true;
            expulsar.push(s.id.clone());
        }
        self.sesiones.push_back(Sesion {
            id: id.to_string(),
            rol,
            sub: sub.to_string(),
            turn_id: turn_id.map(String::from),
            pendiente: false,
        });
        while self.sesiones.len() > MAX_SESIONES {
            if let Some(vieja) = self.sesiones.pop_front() {
                if !vieja.pendiente && !expulsar.contains(&vieja.id) {
                    expulsar.push(vieja.id);
                }
            }
        }
        expulsar
    }

    /// Las sesiones registradas que estos autorizados ya no dejan ver y que no están pendientes; quedan
    /// pendientes (y registradas hasta `quitar`).
    pub(super) fn no_autorizadas(&mut self, a: &Autorizados) -> Vec<String> {
        let mut expulsar = Vec::new();
        for s in self.sesiones.iter_mut().filter(|s| !s.pendiente && !a.video(s.rol, &s.sub, s.turn_id.as_deref())) {
            s.pendiente = true;
            expulsar.push(s.id.clone());
        }
        expulsar
    }

    /// Saca una sesión ya expulsada.
    pub(super) fn quitar(&mut self, id: &str) {
        self.sesiones.retain(|s| s.id != id);
    }

    /// La expulsión falló del todo: la sesión sigue y el próximo `registrar` o `no_autorizadas` la vuelve
    /// a devolver.
    pub(super) fn fallida(&mut self, id: &str) {
        if let Some(s) = self.sesiones.iter_mut().find(|s| s.id == id) {
            s.pendiente = false;
        }
    }

    pub(super) fn len(&self) -> usize {
        self.sesiones.len()
    }

    /// Lo que se guarda en disco: `{"sesiones":[{"id","rol","sub","turnId"}]}` (sin lo pendiente).
    pub(super) fn a_json(&self) -> String {
        let g = Guardado {
            sesiones: self
                .sesiones
                .iter()
                .map(|s| SesionGuardada { id: s.id.clone(), rol: s.rol, sub: s.sub.clone(), turn_id: s.turn_id.clone() })
                .collect(),
        };
        serde_json::to_string(&g).unwrap_or_else(|_| r#"{"sesiones":[]}"#.into())
    }

    /// Lee lo que dejó `a_json`. Algo raro (otro formato, un `id` que no es UUID, el operador, campos
    /// vacíos o largos, más de `MAX_SESIONES`) descarta todo.
    pub(super) fn desde_json(texto: &str) -> Result<Self, ()> {
        let g: Guardado = serde_json::from_str(texto).map_err(|_| ())?;
        let valido = |t: &str| !t.is_empty() && t.len() <= MAX_SUB;
        let bien = g.sesiones.len() <= MAX_SESIONES
            && g.sesiones.iter().all(|s| {
                es_uuid(&s.id) && s.rol != Rol::Operador && valido(&s.sub) && s.turn_id.as_deref().is_none_or(valido)
            });
        if !bien {
            return Err(());
        }
        let sesiones = g
            .sesiones
            .into_iter()
            .map(|s| Sesion { id: s.id, rol: s.rol, sub: s.sub, turn_id: s.turn_id, pendiente: false })
            .collect();
        Ok(Self { sesiones })
    }
}

/// El registro guardado al arrancar. Sin archivo, vacío. Si está corrupto, se registra, se borra y se
/// arranca vacío (esas sesiones ya no se van a poder expulsar); si no se puede leer, vacío.
pub(super) fn cargar(ruta: &Path) -> SesionesWhep {
    use std::io::Read;
    let mut texto = String::new();
    let leido = std::fs::File::open(ruta).and_then(|f| f.take(MAX_ARCHIVO + 1).read_to_string(&mut texto));
    match leido {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SesionesWhep::default(),
        Err(e) if e.kind() != std::io::ErrorKind::InvalidData => {
            crate::aviso!("whep: no pude leer el registro {} ({e}); arranco sin sesiones", ruta.display());
            SesionesWhep::default()
        }
        _ => match SesionesWhep::desde_json(&texto) {
            Ok(s) if texto.len() as u64 <= MAX_ARCHIVO => {
                crate::info!("whep: {} sesiones del registro anterior", s.len());
                s
            }
            _ => {
                crate::aviso!("whep: el registro {} está corrupto; arranco sin sesiones y lo borro", ruta.display());
                if let Err(e) = std::fs::remove_file(ruta) {
                    crate::aviso!("whep: no pude borrar {}: {e}", ruta.display());
                }
                SesionesWhep::default()
            }
        },
    }
}

/// Guarda el registro (escritura atómica).
pub(super) fn guardar(ruta: &Path, json: &str) -> std::io::Result<()> {
    crate::pausa::escribir_atomico(ruta, json.as_bytes())
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
        // La 1 ya tiene la expulsión en vuelo: no se vuelve a pedir.
        assert_eq!(s.registrar(&uuid(4), Rol::Jugador, "u2", Some("t1")), vec![uuid(3)]);
        s.quitar(&uuid(1));
        s.quitar(&uuid(3));
        let mut todas = s.no_autorizadas(&Autorizados::vacio());
        todas.sort();
        assert_eq!(todas, vec![uuid(2), uuid(4)]);
    }

    #[test]
    fn una_expulsion_pendiente_no_se_devuelve_dos_veces_y_si_falla_vuelve() {
        let mut s = SesionesWhep::default();
        s.registrar(&uuid(1), Rol::Jugador, "u1", Some("t1"));
        assert_eq!(s.no_autorizadas(&Autorizados::vacio()), vec![uuid(1)]);
        assert!(s.no_autorizadas(&Autorizados::vacio()).is_empty(), "ya está pendiente");
        assert!(s.registrar(&uuid(2), Rol::Jugador, "u1", Some("t1")).is_empty(), "pendiente: tampoco por la misma cuenta");
        s.fallida(&uuid(1));
        assert_eq!(s.registrar(&uuid(3), Rol::Jugador, "u1", Some("t1")), vec![uuid(1), uuid(2)], "falló: se vuelve a pedir");
        s.fallida(&uuid(9));
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
    fn con_mas_de_64_la_mas_vieja_sale_y_se_expulsa() {
        let mut s = SesionesWhep::default();
        for n in 0..MAX_SESIONES {
            assert!(s.registrar(&uuid(n), Rol::Vista, &format!("u{n}"), None).is_empty());
        }
        assert_eq!(s.registrar(&uuid(MAX_SESIONES), Rol::Vista, "otra", None), vec![uuid(0)]);
        let quedan = s.no_autorizadas(&Autorizados::vacio());
        assert_eq!(quedan.len(), MAX_SESIONES);
        assert!(!quedan.contains(&uuid(0)), "la más vieja ya no está");
        assert!(quedan.contains(&uuid(MAX_SESIONES)));
    }

    #[test]
    fn el_registro_en_disco_ida_y_vuelta_sin_lo_pendiente() {
        let mut s = SesionesWhep::default();
        s.registrar(&uuid(1), Rol::Jugador, "u1", Some("t1"));
        s.registrar(&uuid(2), Rol::Vista, "u2", None);
        let texto = s.a_json();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&texto).unwrap(),
            serde_json::json!({"sesiones":[
                {"id":uuid(1),"rol":"jugador","sub":"u1","turnId":"t1"},
                {"id":uuid(2),"rol":"vista","sub":"u2","turnId":null}
            ]})
        );
        assert_eq!(s.no_autorizadas(&Autorizados::vacio()).len(), 2);
        assert_eq!(s.a_json(), texto, "marcar pendiente no cambia lo que se guarda");
        let mut leido = SesionesWhep::desde_json(&texto).unwrap();
        assert_eq!(leido.a_json(), texto);
        let mut ids = leido.no_autorizadas(&Autorizados::vacio());
        ids.sort();
        assert_eq!(ids, vec![uuid(1), uuid(2)], "al cargar no hay nada pendiente");
    }

    #[test]
    fn un_registro_en_disco_raro_se_rechaza_entero() {
        let una = |id: &str, rol: &str, sub: &str| {
            serde_json::json!({"sesiones":[{"id":id,"rol":rol,"sub":sub,"turnId":null}]}).to_string()
        };
        assert!(SesionesWhep::desde_json(&una(&uuid(1), "vista", "u2")).is_ok());
        let largo = "x".repeat(65);
        for malo in [
            String::new(),
            "no es json".into(),
            "[]".into(),
            r#"{"sesiones":[],"otra":1}"#.into(),
            una("no-es-uuid", "vista", "u2"),
            una(&uuid(1), "operador", "op"),
            una(&uuid(1), "rey", "u2"),
            una(&uuid(1), "vista", ""),
            una(&uuid(1), "vista", &largo),
            serde_json::json!({"sesiones":[{"id":uuid(1),"rol":"jugador","sub":"u1","turnId":largo}]}).to_string(),
            serde_json::json!({"sesiones":(0..=MAX_SESIONES).map(|n| serde_json::json!({"id":uuid(n),"rol":"vista","sub":format!("u{n}"),"turnId":null})).collect::<Vec<_>>()}).to_string(),
        ] {
            assert!(SesionesWhep::desde_json(&malo).is_err(), "{malo}");
        }
    }
}

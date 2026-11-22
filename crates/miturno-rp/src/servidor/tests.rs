//! Tests del servidor, todo en memoria y con el reloj de tokio pausado: los pedidos HTTP van por
//! `oneshot` (con la IP de origen simulada) y el WebSocket por un dúplex servido con `axum::serve`.

use super::*;
use crate::ciclo::Orden;
use crate::metricas::Metricas;
use crate::sesion::falsa::SesionFalsa;
use crate::sesion::ConLimite;
use crate::sistema::{Futuro, Sistema, TiemposCpu};
use axum::body::{to_bytes, Body};
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::SigningKey;
use futures_util::{SinkExt, StreamExt};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use rp_core::mando::{CREATE, CRUZ, L1, OPTIONS, PS};
use rp_core::EstadoMando;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use tokio::time::{sleep, Instant};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::WebSocketStream;
use tower::ServiceExt;

const AHORA: u64 = 1_800_000_000;
const TOKEN_CONTROL: &str = "token-de-control-de-prueba-0123456789abcdef";

fn local() -> SocketAddr {
    "127.0.0.1:50000".parse().unwrap()
}
fn remota() -> SocketAddr {
    "203.0.113.9:50000".parse().unwrap()
}

/// Par de claves armado en el test (nunca se versiona una clave): (privada PKCS8, pública SPKI).
fn par(semilla: u8) -> (String, String) {
    let sk = SigningKey::from_bytes(&[semilla; 32]);
    (
        sk.to_pkcs8_pem(LineEnding::LF).unwrap().to_string(),
        sk.verifying_key().to_public_key_pem(LineEnding::LF).unwrap(),
    )
}

/// Claims de una credencial de `u1`; la de jugador, del turno `t1`.
fn claims(rol: &str) -> Value {
    let mut c = json!({"iss":"miturno","aud":"miturno-video","sub":"u1","rol":rol,"iat":AHORA - 10,"exp":AHORA + 3600,"jti":"j1"});
    if rol == "jugador" {
        c["turnId"] = json!("t1");
    }
    c
}

/// Credencial de jugador de la cuenta `sub` para el turno `turno`.
fn token_jugador(sub: &str, turno: &str) -> String {
    let mut c = claims("jugador");
    c["sub"] = json!(sub);
    c["turnId"] = json!(turno);
    firmar(&c)
}

/// Credencial de vista de la cuenta `sub` (sin `turnId`).
fn token_vista(sub: &str) -> String {
    let mut c = claims("vista");
    c["sub"] = json!(sub);
    firmar(&c)
}

/// Credencial de operador de la cuenta `sub`.
fn token_operador(sub: &str) -> String {
    let mut c = claims("operador");
    c["sub"] = json!(sub);
    firmar(&c)
}

/// Un `id` de sesión WHEP como los de MediaMTX (UUID en minúsculas).
fn uuid(n: u32) -> String {
    format!("{n:08x}-0000-4000-8000-{n:012x}")
}

/// Cuerpo de `/control/turno` con vistas.
fn cuerpo_autorizados(version: u64, jugador: Option<(&str, &str)>, vistas: &[&str]) -> String {
    let j = jugador.map(|(sub, t)| json!({"sub":sub,"turnId":t})).unwrap_or(Value::Null);
    json!({"version":version,"jugador":j,"vistas":vistas}).to_string()
}

/// Cuerpo de `/control/turno` sin vistas.
fn cuerpo_turno(version: u64, jugador: Option<(&str, &str)>) -> String {
    let j = jugador.map(|(sub, t)| json!({"sub":sub,"turnId":t})).unwrap_or(Value::Null);
    json!({"version":version,"jugador":j,"vistas":[]}).to_string()
}

fn firmar(c: &Value) -> String {
    let k = EncodingKey::from_ed_pem(par(1).0.as_bytes()).unwrap();
    encode(&Header::new(Algorithm::EdDSA), c, &k).unwrap()
}

fn token(rol: &str) -> String {
    firmar(&claims(rol))
}

/// Sistema falso: estados de unidades fijos, registro de `systemctl` y una cola de muestras de CPU.
#[derive(Default)]
struct SistemaFalso {
    estados: Mutex<HashMap<String, String>>,
    llamadas: Mutex<Vec<(String, String)>>,
    falla_systemctl: Mutex<bool>,
    cpu: Mutex<VecDeque<TiemposCpu>>,
    /// Cada `expulsar_whep` pedido (también los que fallan), en orden y con la hora.
    expulsiones: Mutex<Vec<(String, Instant)>>,
    /// Cuántas de las próximas expulsiones fallan.
    fallas: Mutex<u32>,
    /// Lo que tarda cada expulsión (como un `curl` lento).
    demora: Mutex<Duration>,
    /// Expulsiones en curso ahora y el máximo que hubo a la vez.
    en_vuelo: Arc<Mutex<(u32, u32)>>,
}

impl Sistema for SistemaFalso {
    fn estados<'a>(&'a self, unidades: &'a [&'a str]) -> Futuro<'a, Vec<String>> {
        let e = self.estados.lock().unwrap();
        let v = unidades.iter().map(|u| e.get(*u).cloned().unwrap_or_else(|| "inactive".into())).collect();
        Box::pin(std::future::ready(v))
    }
    fn systemctl<'a>(&'a self, verbo: &'a str, unidad: &'a str) -> Futuro<'a, Result<(), String>> {
        self.llamadas.lock().unwrap().push((verbo.into(), unidad.into()));
        let r = if *self.falla_systemctl.lock().unwrap() { Err("polkit dijo que no".into()) } else { Ok(()) };
        Box::pin(std::future::ready(r))
    }
    fn expulsar_whep<'a>(&'a self, id: &'a str) -> Futuro<'a, Result<(), String>> {
        self.expulsiones.lock().unwrap().push((id.into(), Instant::now()));
        let r = {
            let mut f = self.fallas.lock().unwrap();
            if *f > 0 {
                *f -= 1;
                Err("la API de MediaMTX no contesta".to_string())
            } else {
                Ok(())
            }
        };
        let demora = *self.demora.lock().unwrap();
        let en_vuelo = self.en_vuelo.clone();
        Box::pin(async move {
            {
                let mut v = en_vuelo.lock().unwrap();
                v.0 += 1;
                v.1 = v.1.max(v.0);
            }
            sleep(demora).await;
            en_vuelo.lock().unwrap().0 -= 1;
            r
        })
    }
    fn tiempos_cpu(&self) -> Option<TiemposCpu> {
        let mut c = self.cpu.lock().unwrap();
        if c.len() > 1 {
            c.pop_front()
        } else {
            c.front().copied()
        }
    }
}

struct Banco {
    estado: Arc<EstadoServidor<SesionFalsa>>,
    sesion: Arc<ConLimite<SesionFalsa>>,
    sistema: Arc<SistemaFalso>,
    ordenes: mpsc::Receiver<Orden>,
    metricas: Arc<Metricas>,
    conexiones: mpsc::Sender<(DuplexStream, SocketAddr)>,
}

/// Banco con el turno `(u1, t1)` ya empujado (versión 1): el de `token("jugador")`.
async fn banco_con_turno() -> Banco {
    let b = banco();
    b.empujar(1, Some(("u1", "t1"))).await;
    b
}

fn banco() -> Banco {
    banco_con(None)
}

/// Banco con el registro de sesiones WHEP guardado en `whep` (como el de `/var/lib/miturno-rp`).
fn banco_con(whep: Option<std::path::PathBuf>) -> Banco {
    let sesion = Arc::new(ConLimite::new(SesionFalsa::new(), 3_000));
    let sistema = Arc::new(SistemaFalso::default());
    let (tx_ordenes, ordenes) = mpsc::channel(16);
    let metricas = Arc::new(Metricas::new());
    let estado = EstadoServidor::new(Piezas {
        sesion: sesion.clone(),
        clave: rp_core::ClaveVerificacion::desde_pem(&par(1).1).unwrap(),
        token_control: TokenControl::new(TOKEN_CONTROL).unwrap(),
        ordenes: tx_ordenes,
        metricas: metricas.clone(),
        sistema: sistema.clone(),
        reloj_s: || AHORA,
        sesiones_whep: whep,
    });
    let (conexiones, rx) = mpsc::channel(8);
    tokio::spawn(servir(Memoria(rx), estado.clone()));
    Banco { estado, sesion, sistema, ordenes, metricas, conexiones }
}

/// Listener en memoria: cada conexión es un dúplex con una IP de origen elegida por el test.
struct Memoria(mpsc::Receiver<(DuplexStream, SocketAddr)>);

impl axum::serve::Listener for Memoria {
    type Io = DuplexStream;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (DuplexStream, SocketAddr) {
        match self.0.recv().await {
            Some(c) => c,
            None => std::future::pending().await,
        }
    }
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok("127.0.0.1:7070".parse().unwrap())
    }
}

type Ws = WebSocketStream<DuplexStream>;

impl Banco {
    fn mandos(&self) -> Vec<EstadoMando> {
        self.sesion.interna().registro().mandos
    }
    fn ultimo_mando(&self) -> EstadoMando {
        *self.mandos().last().expect("ningún mando")
    }

    async fn ws(&self) -> Ws {
        let (cli, srv) = tokio::io::duplex(64 * 1024);
        self.conexiones.send((srv, local())).await.unwrap();
        tokio_tungstenite::client_async("ws://127.0.0.1:7070/ctl", cli).await.expect("handshake de /ctl").0
    }

    /// Conecta y se autentica; devuelve la respuesta del servidor.
    async fn ws_con(&self, token: &str) -> (Ws, Value) {
        let mut ws = self.ws().await;
        enviar(&mut ws, json!({"t":"auth","token":token})).await;
        let r = recibir(&mut ws).await.expect("respuesta a auth");
        (ws, r)
    }

    async fn pedir(&self, metodo: &str, uri: &str, desde: SocketAddr, auth: Option<&str>, cuerpo: &str) -> (StatusCode, String) {
        let mut req = Request::builder().method(metodo).uri(uri).header("content-type", "application/json");
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        let r = rutas(self.estado.clone())
            .layer(MockConnectInfo(desde))
            .oneshot(req.body(Body::from(cuerpo.to_string())).unwrap())
            .await
            .unwrap();
        let st = r.status();
        let b = to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (st, String::from_utf8(b.to_vec()).unwrap())
    }

    async fn control(&self, metodo: &str, accion: &str) -> (StatusCode, String) {
        let bearer = format!("Bearer {TOKEN_CONTROL}");
        self.pedir(metodo, &format!("/control/{accion}"), local(), Some(&bearer), "").await
    }

    /// `POST /control/turno` con el token de control.
    async fn turno(&self, cuerpo: &str) -> (StatusCode, String) {
        let bearer = format!("Bearer {TOKEN_CONTROL}");
        self.pedir("POST", "/control/turno", local(), Some(&bearer), cuerpo).await
    }

    /// Empuja el turno vigente y exige `200 {"ok":true}`.
    async fn empujar(&self, version: u64, jugador: Option<(&str, &str)>) {
        let (st, cuerpo) = self.turno(&cuerpo_turno(version, jugador)).await;
        assert_eq!((st, cuerpo.as_str()), (StatusCode::OK, r#"{"ok":true}"#), "empuje v{version}");
    }

    /// `jugador` de la salud.
    async fn salud_jugador(&self) -> Value {
        let (st, cuerpo) = self.control("GET", "salud").await;
        assert_eq!(st, StatusCode::OK);
        serde_json::from_str::<Value>(&cuerpo).unwrap()["jugador"].clone()
    }

    async fn auth_mediamtx(&self, cuerpo: Value) -> StatusCode {
        self.pedir("POST", "/interno/auth", local(), None, &cuerpo.to_string()).await.0
    }

    /// Empuja el turno vigente con vistas y exige `200 {"ok":true}`.
    async fn empujar_con(&self, version: u64, jugador: Option<(&str, &str)>, vistas: &[&str]) {
        let (st, cuerpo) = self.turno(&cuerpo_autorizados(version, jugador, vistas)).await;
        assert_eq!((st, cuerpo.as_str()), (StatusCode::OK, r#"{"ok":true}"#), "empuje v{version}");
    }

    /// Lo que contesta `/interno/auth` a un WHEP de `ps5` con esa credencial y ese `id` de sesión.
    async fn ver(&self, token: &str, id: Option<&str>) -> StatusCode {
        self.auth_mediamtx(whep(token, id)).await
    }

    /// Las expulsiones pedidas hasta ahora (después de dejar correr las tareas en segundo plano).
    async fn expulsiones(&self) -> Vec<String> {
        sleep(Duration::from_millis(5)).await;
        self.sistema.expulsiones.lock().unwrap().iter().map(|(id, _)| id.clone()).collect()
    }

    /// Cuándo se pidió cada expulsión, en ms desde `t0`.
    fn momentos_ms(&self, t0: Instant) -> Vec<u64> {
        self.sistema.expulsiones.lock().unwrap().iter().map(|(_, t)| (*t - t0).as_millis() as u64).collect()
    }
}

async fn enviar(ws: &mut Ws, v: Value) {
    ws.send(WsMsg::text(v.to_string())).await.unwrap();
}

/// El próximo mensaje de texto del servidor, o `None` si cerró.
async fn recibir(ws: &mut Ws) -> Option<Value> {
    loop {
        match ws.next().await {
            Some(Ok(WsMsg::Text(t))) => return Some(serde_json::from_str(t.as_str()).unwrap()),
            Some(Ok(WsMsg::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => continue,
        }
    }
}

/// El servidor cerró la conexión (sin mandar nada más).
async fn cerrado(ws: &mut Ws) -> bool {
    tokio::time::timeout(Duration::from_millis(100), recibir(ws)).await.is_ok_and(|r| r.is_none())
}

/// Credencial ya validada de `u1` (la de jugador, del turno `t1`).
fn cred(rol: rp_core::Rol) -> rp_core::Credencial {
    let turn_id = (rol == rp_core::Rol::Jugador).then(|| "t1".to_string());
    rp_core::Credencial { sub: "u1".into(), rol, turn_id, exp: AHORA + 3600, jti: "j1".into() }
}

fn estado_json(botones: u32) -> Value {
    json!({"t":"estado","botones":botones,"l2":0,"r2":0,"lx":100,"ly":-100,"rx":0,"ry":0})
}

fn con(botones: u32) -> EstadoMando {
    EstadoMando { botones, l2: 0, r2: 0, lx: 100, ly: -100, rx: 0, ry: 0 }
}

// ---------------------------------------------------------------- /ctl

#[tokio::test(start_paused = true)]
async fn ctl_sin_auth_en_2_s_cierra_con_formato() {
    let b = banco();
    let mut ws = b.ws().await;
    let t = Instant::now();
    assert_eq!(recibir(&mut ws).await, Some(json!({"t":"error","codigo":"FORMATO"})));
    let tardo = t.elapsed();
    assert!(tardo >= Duration::from_secs(2) && tardo < Duration::from_millis(2_100), "{tardo:?}");
    assert!(cerrado(&mut ws).await);
}

#[tokio::test(start_paused = true)]
async fn ctl_primer_mensaje_que_no_es_auth_es_formato() {
    let b = banco();
    let mut ws = b.ws().await;
    enviar(&mut ws, json!({"t":"latido"})).await;
    assert_eq!(recibir(&mut ws).await, Some(json!({"t":"error","codigo":"FORMATO"})));
    assert!(cerrado(&mut ws).await);
    assert!(b.mandos().is_empty());
}

#[tokio::test(start_paused = true)]
async fn ctl_token_de_otra_audiencia_es_audiencia() {
    let b = banco();
    let mut c = claims("operador");
    c["aud"] = json!("otra-cosa");
    let (mut ws, r) = b.ws_con(&firmar(&c)).await;
    assert_eq!(r, json!({"t":"error","codigo":"AUDIENCIA"}));
    assert!(cerrado(&mut ws).await);
}

#[tokio::test(start_paused = true)]
async fn ctl_token_firmado_con_otra_clave_es_firma() {
    let b = banco();
    let k = EncodingKey::from_ed_pem(par(2).0.as_bytes()).unwrap();
    let t = encode(&Header::new(Algorithm::EdDSA), &claims("operador"), &k).unwrap();
    let (_, r) = b.ws_con(&t).await;
    assert_eq!(r, json!({"t":"error","codigo":"FIRMA"}));
}

#[tokio::test(start_paused = true)]
async fn ctl_operador_valido_recibe_ok_con_su_rol() {
    let b = banco();
    let (_ws, r) = b.ws_con(&token("operador")).await;
    assert_eq!(r, json!({"t":"ok","rol":"operador"}));
}

#[tokio::test(start_paused = true)]
async fn ctl_estado_con_ps_de_operador_llega_a_la_sesion() {
    let b = banco();
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws, estado_json(PS | CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(PS | CRUZ));
}

#[tokio::test(start_paused = true)]
async fn ctl_el_filtro_del_jugador_se_aplica_antes_de_la_sesion() {
    let b = banco_con_turno().await;
    let (mut ws, r) = b.ws_con(&token("jugador")).await;
    assert_eq!(r, json!({"t":"ok","rol":"jugador"}));
    enviar(&mut ws, estado_json(PS | CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(CRUZ), "PS nunca llega a la PS5 desde un jugador");
}

#[tokio::test(start_paused = true)]
async fn ctl_silencio_de_600_ms_suelta_todo_sin_cerrar() {
    let b = banco();
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(450)).await;
    assert_eq!(b.ultimo_mando(), con(CRUZ), "a los 450 ms sigue apretado");
    sleep(Duration::from_millis(150)).await;
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto(), "a los 600 ms, todo suelto");
    // La conexión sigue: un estado nuevo vuelve a pasar.
    enviar(&mut ws, estado_json(L1)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(L1));
}

#[tokio::test(start_paused = true)]
async fn ctl_tras_un_hipo_de_red_el_latido_vuelve_a_apretar_lo_mantenido() {
    let b = banco();
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    // Cruz mantenida: el cliente solo manda latidos mientras no cambia nada.
    enviar(&mut ws, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(600)).await;
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto(), "600 ms de silencio: todo suelto");
    enviar(&mut ws, json!({"t":"latido"})).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(CRUZ), "vuelve la conexión: Cruz sigue apretada");
    // Un segundo latido no repite el envío (ya no está suelto por silencio).
    let antes = b.mandos().len();
    enviar(&mut ws, json!({"t":"latido"})).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.mandos().len(), antes);
    // Un pedido de keyframe tras otro hueco también restaura.
    sleep(Duration::from_millis(600)).await;
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto());
    enviar(&mut ws, json!({"t":"keyframe"})).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(CRUZ));
}

#[tokio::test(start_paused = true)]
async fn ctl_si_el_jugador_solto_durante_el_hueco_el_estado_nuevo_manda() {
    let b = banco();
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(600)).await;
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto());
    // TCP entrega en orden: el estado nuevo (soltó Cruz) llega antes que el latido.
    enviar(&mut ws, estado_json(0)).await;
    enviar(&mut ws, json!({"t":"latido"})).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(0));
    assert!(b.mandos().iter().rev().take_while(|m| **m != EstadoMando::suelto()).all(|m| m.botones & CRUZ == 0));
}

#[tokio::test(start_paused = true)]
async fn ctl_el_jugador_restaurado_tampoco_pasa_ps() {
    let b = banco_con_turno().await;
    let (mut ws, _) = b.ws_con(&token("jugador")).await;
    enviar(&mut ws, estado_json(CRUZ | PS)).await;
    sleep(Duration::from_millis(600)).await;
    enviar(&mut ws, json!({"t":"latido"})).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(CRUZ));
}

#[tokio::test(start_paused = true)]
async fn ctl_los_latidos_mantienen_el_estado() {
    let b = banco();
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws, estado_json(CRUZ)).await;
    for _ in 0..10 {
        sleep(Duration::from_millis(300)).await;
        enviar(&mut ws, json!({"t":"latido"})).await;
    }
    sleep(Duration::from_millis(5)).await;
    let m = b.mandos();
    let desde = m.iter().position(|e| *e == con(CRUZ)).unwrap();
    assert!(m[desde..].iter().all(|e| *e == con(CRUZ)), "3 s de latidos sin soltar: {m:?}");
}

#[tokio::test(start_paused = true)]
async fn ctl_credencial_que_vence_a_los_3_s_suelta_y_cierra() {
    let b = banco();
    // Con la tolerancia de 30 s, `exp = ahora − 27` vence a los 3 s (lo mismo que haría `validar`).
    let mut c = claims("operador");
    c["exp"] = json!(AHORA - 27);
    let (mut ws, r) = b.ws_con(&firmar(&c)).await;
    assert_eq!(r, json!({"t":"ok","rol":"operador"}));
    let t = Instant::now();
    enviar(&mut ws, estado_json(CRUZ)).await;
    let mut ultimo = None;
    // Latidos cada 200 ms: el suelto no es por silencio.
    while t.elapsed() < Duration::from_millis(3_500) {
        match tokio::time::timeout(Duration::from_millis(200), recibir(&mut ws)).await {
            Ok(m) => {
                ultimo = m;
                break;
            }
            Err(_) => {
                assert_eq!(b.ultimo_mando(), con(CRUZ));
                enviar(&mut ws, json!({"t":"latido"})).await;
            }
        }
    }
    let tardo = t.elapsed();
    assert_eq!(ultimo, Some(json!({"t":"error","codigo":"VENCIDA"})));
    assert!(tardo >= Duration::from_secs(3) && tardo < Duration::from_millis(3_100), "{tardo:?}");
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto());
    assert!(cerrado(&mut ws).await);
}

#[tokio::test(start_paused = true)]
async fn ctl_la_tolerancia_de_30_s_vale_tambien_con_la_conexion_abierta() {
    let b = banco();
    let mut c = claims("operador");
    c["exp"] = json!(AHORA + 3);
    let (mut ws, _) = b.ws_con(&firmar(&c)).await;
    let t = Instant::now();
    assert_eq!(recibir(&mut ws).await, Some(json!({"t":"error","codigo":"VENCIDA"})));
    let tardo = t.elapsed();
    assert!(tardo >= Duration::from_secs(33) && tardo < Duration::from_millis(33_100), "{tardo:?}");
}

#[tokio::test(start_paused = true)]
async fn ctl_una_segunda_conexion_valida_reemplaza_a_la_primera() {
    let b = banco();
    let (mut ws1, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws1, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(CRUZ));

    let (mut ws2, r2) = b.ws_con(&token("operador")).await;
    assert_eq!(r2, json!({"t":"ok","rol":"operador"}));
    assert_eq!(recibir(&mut ws1).await, Some(json!({"t":"reemplazado","por":"operador"})));
    assert!(cerrado(&mut ws1).await);
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto(), "al reemplazar se suelta lo de la anterior");

    enviar(&mut ws2, estado_json(PS)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(PS), "la nueva manda");
}

#[tokio::test(start_paused = true)]
async fn ctl_la_conexion_reemplazada_ya_no_manda() {
    let b = banco();
    let (mut ws1, _) = b.ws_con(&token("operador")).await;
    let (mut ws2, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws2, estado_json(PS)).await;
    // La vieja manda algo antes de enterarse: no llega a la PS5.
    let _ = ws1.send(WsMsg::text(estado_json(CRUZ).to_string())).await;
    sleep(Duration::from_millis(5)).await;
    assert!(!b.mandos().contains(&con(CRUZ)), "{:?}", b.mandos());
    assert_eq!(b.ultimo_mando(), con(PS));
}

#[tokio::test(start_paused = true)]
async fn ctl_un_rol_menor_no_reemplaza_y_recibe_ocupado() {
    let b = banco_con_turno().await;
    let (mut ws1, _) = b.ws_con(&token("operador")).await;
    let (_ws2, r) = b.ws_con(&token("jugador")).await;
    assert_eq!(r, json!({"t":"error","codigo":"OCUPADO"}));
    enviar(&mut ws1, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(CRUZ), "el operador sigue mandando");
}

#[tokio::test(start_paused = true)]
async fn ctl_al_cerrar_la_conexion_suelta_todo() {
    let b = banco();
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    ws.close(None).await.unwrap();
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto());
}

#[tokio::test(start_paused = true)]
async fn ctl_mensaje_invalido_suelta_y_cierra_con_formato() {
    let b = banco();
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws, estado_json(CRUZ)).await;
    enviar(&mut ws, json!({"t":"estado","botones":65536,"l2":0,"r2":0,"lx":0,"ly":0,"rx":0,"ry":0})).await;
    assert_eq!(recibir(&mut ws).await, Some(json!({"t":"error","codigo":"FORMATO"})));
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto());
    assert!(cerrado(&mut ws).await);
}

#[tokio::test(start_paused = true)]
async fn ctl_options_del_jugador_sigue_apretado_sin_soltarse_a_los_2_s() {
    let b = banco_con_turno().await;
    let (mut ws, _) = b.ws_con(&token("jugador")).await;
    enviar(&mut ws, estado_json(OPTIONS)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(OPTIONS));
    for _ in 0..15 {
        sleep(Duration::from_millis(200)).await;
        enviar(&mut ws, json!({"t":"latido"})).await;
    }
    sleep(Duration::from_millis(5)).await; // 3 s, solo latidos
    assert_eq!(b.ultimo_mando(), con(OPTIONS), "Options es libre: nadie lo fuerza a suelto");
}

#[tokio::test(start_paused = true)]
async fn ctl_keyframe_pasa_por_el_limitador_unico() {
    let b = banco();
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws, json!({"t":"keyframe"})).await;
    enviar(&mut ws, json!({"t":"keyframe"})).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.sesion.interna().registro().keyframes, 1);
    // El mismo limitador que /interno/keyframe.
    let (st, cuerpo) = b.pedir("POST", "/interno/keyframe", local(), None, "").await;
    assert_eq!((st, cuerpo.as_str()), (StatusCode::OK, r#"{"enviado":false}"#));
    // Los dos rechazados (el segundo de /ctl y el de /interno) no se pierden: salen juntos, en un
    // solo pedido diferido, al final de la ventana de 3 s.
    sleep(Duration::from_secs(3)).await;
    assert_eq!(b.sesion.interna().registro().keyframes, 2);
    enviar(&mut ws, json!({"t":"keyframe"})).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.sesion.interna().registro().keyframes, 2, "el diferido corrió la ventana");
    sleep(Duration::from_secs(3)).await;
    assert_eq!(b.sesion.interna().registro().keyframes, 3);
}

#[tokio::test(start_paused = true)]
async fn ctl_sin_ruta_de_websocket_en_otro_lado() {
    let b = banco();
    let (st, _) = b.pedir("GET", "/otra", local(), None, "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test(start_paused = true)]
async fn ranura_solo_el_dueno_manda_y_suelta() {
    let b = banco_con_turno().await;
    let e = &b.estado;
    let _r1 = e.tomar(1, &cred(rp_core::Rol::Operador)).unwrap();
    let _r2 = e.tomar(2, &cred(rp_core::Rol::Operador)).unwrap();
    let antes = b.mandos().len();
    e.mandar(1, &con(CRUZ));
    e.soltar(1);
    assert_eq!(b.mandos().len(), antes, "la conexión reemplazada ni manda ni suelta");
    e.mandar(2, &con(PS));
    assert_eq!(b.ultimo_mando(), con(PS));
    assert_eq!(e.tomar(3, &cred(rp_core::Rol::Jugador)).err(), Some("OCUPADO"), "un rol menor no toma");
    e.soltar(2);
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto());
    assert!(e.tomar(3, &cred(rp_core::Rol::Jugador)).is_ok(), "libre, el jugador del turno toma");
}

#[tokio::test(start_paused = true)]
async fn ctl_un_panico_en_la_conexion_libera_la_ranura_y_suelta_una_vez() {
    let b = banco_con_turno().await;
    b.estado.panico_en_estado.store(true, std::sync::atomic::Ordering::SeqCst);
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    let m = b.mandos();
    let i = m.iter().position(|e| *e == con(CRUZ)).expect("llegó el estado antes del pánico");
    assert_eq!(&m[i + 1..], &[EstadoMando::suelto()], "suelto exactamente una vez: {m:?}");
    assert!(b.estado.ranura.lock().unwrap().ocupante.is_none(), "la ranura quedó libre");
    // Libre: hasta un rol menor la puede tomar (no recibe OCUPADO).
    b.estado.panico_en_estado.store(false, std::sync::atomic::Ordering::SeqCst);
    let (_ws2, r) = b.ws_con(&token("jugador")).await;
    assert_eq!(r, json!({"t":"ok","rol":"jugador"}));
}

#[tokio::test(start_paused = true)]
async fn ranura_una_tarea_cancelada_libera_y_suelta_una_vez() {
    let b = banco();
    let e = b.estado.clone();
    let tarea = tokio::spawn(async move {
        let (_dueno, _aviso) = e.tomar(7, &cred(rp_core::Rol::Operador)).unwrap();
        e.mandar(7, &con(CRUZ));
        std::future::pending::<()>().await;
    });
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(CRUZ));
    tarea.abort();
    assert!(tarea.await.unwrap_err().is_cancelled());
    let m = b.mandos();
    let i = m.iter().position(|e| *e == con(CRUZ)).unwrap();
    assert_eq!(&m[i + 1..], &[EstadoMando::suelto()], "suelto exactamente una vez: {m:?}");
    assert!(b.estado.ranura.lock().unwrap().ocupante.is_none());
}

#[tokio::test(start_paused = true)]
async fn ctl_exp_enorme_no_entra_en_panico_y_se_corta_al_tope() {
    let b = banco();
    let mut c = claims("operador");
    c["exp"] = json!(u64::MAX);
    let (mut ws, r) = b.ws_con(&firmar(&c)).await;
    assert_eq!(r, json!({"t":"ok","rol":"operador"}));
    enviar(&mut ws, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(CRUZ), "la conexión sigue viva");
    let t = Instant::now();
    assert_eq!(recibir(&mut ws).await, Some(json!({"t":"error","codigo":"VENCIDA"})));
    let tardo = t.elapsed();
    assert!(tardo >= ctl::TOPE_CONEXION - Duration::from_millis(10) && tardo <= ctl::TOPE_CONEXION, "{tardo:?}");
}

/// Intenta el handshake de `/ctl`: `Err` con el código HTTP si el servidor no lo acepta.
async fn handshake(b: &Banco) -> Result<Ws, u16> {
    handshake_desde(b, local(), None).await
}

/// Como `handshake`, desde la IP de par `desde` y, si hay, con el `X-Forwarded-For` que pone Caddy.
async fn handshake_desde(b: &Banco, desde: SocketAddr, xff: Option<&str>) -> Result<Ws, u16> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let (cli, srv) = tokio::io::duplex(16 * 1024);
    b.conexiones.send((srv, desde)).await.unwrap();
    let mut pedido = "ws://127.0.0.1:7070/ctl".into_client_request().unwrap();
    if let Some(x) = xff {
        pedido.headers_mut().insert("x-forwarded-for", x.parse().unwrap());
    }
    match tokio_tungstenite::client_async(pedido, cli).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
        Err(e) => panic!("handshake: {e}"),
    }
}

/// La IP número `n` de un rango de documentación (TEST-NET-2 y TEST-NET-3), distinta para cada `n < 500`.
fn ip_n(n: usize) -> String {
    format!("{}.{}", if n < 250 { "198.51.100" } else { "203.0.113" }, n % 250 + 1)
}

/// Llena el tope global con conexiones sin autenticar, `MAX_SIN_AUTH_POR_IP` por cada IP.
async fn llenar_tope_global(b: &Banco, colgadas: &mut Vec<Ws>) {
    for i in 0..ctl::MAX_SIN_AUTH {
        let ip = ip_n(i / ctl::MAX_SIN_AUTH_POR_IP);
        colgadas.push(handshake_desde(b, local(), Some(&ip)).await.expect("dentro del tope"));
    }
}

#[tokio::test(start_paused = true)]
async fn ctl_con_el_tope_global_de_conexiones_esperando_auth_la_siguiente_recibe_503() {
    // /ctl es público: muchas conexiones que nunca se autentican no pueden juntarse sin tope, aunque
    // vengan de muchas IP.
    let b = banco();
    let mut colgadas = Vec::new();
    llenar_tope_global(&b, &mut colgadas).await;
    assert_eq!(handshake_desde(&b, local(), Some("192.0.2.200")).await.err(), Some(503), "IP nueva, sin lugar global");
    assert_eq!(handshake(&b).await.err(), Some(503), "directo, sin lugar global");
    // Al vencer el plazo de auth se cierran y liberan el lugar.
    sleep(ctl::PLAZO_AUTH + Duration::from_millis(50)).await;
    colgadas.clear();
    let mut ws = handshake_desde(&b, local(), Some("192.0.2.200")).await.expect("con lugar de nuevo");
    enviar(&mut ws, json!({"t":"auth","token":token("operador")})).await;
    assert_eq!(recibir(&mut ws).await, Some(json!({"t":"ok","rol":"operador"})));
    // Una conexión ya autenticada no ocupa lugar: el tope se llena entero otra vez.
    llenar_tope_global(&b, &mut colgadas).await;
    assert_eq!(handshake_desde(&b, local(), Some("192.0.2.201")).await.err(), Some(503));
}

#[tokio::test(start_paused = true)]
async fn ctl_una_ip_con_muchas_conexiones_sin_auth_no_deja_afuera_a_otra() {
    // Con un solo tope global, ~16 conexiones por segundo desde una IP dejaban afuera al operador.
    let b = banco();
    let mut colgadas = Vec::new();
    for _ in 0..ctl::MAX_SIN_AUTH_POR_IP {
        colgadas.push(handshake_desde(&b, local(), Some("198.51.100.7")).await.expect("dentro del tope de la IP"));
    }
    assert_eq!(handshake_desde(&b, local(), Some("198.51.100.7")).await.err(), Some(503));
    // Caddy agrega la IP del cliente al final: lo que mande el cliente adelante no la cambia.
    assert_eq!(handshake_desde(&b, local(), Some("192.0.2.1, 198.51.100.7")).await.err(), Some(503));
    let mut ws = handshake_desde(&b, local(), Some("198.51.100.8")).await.expect("otra IP entra");
    enviar(&mut ws, json!({"t":"auth","token":token("operador")})).await;
    assert_eq!(recibir(&mut ws).await, Some(json!({"t":"ok","rol":"operador"})));
    // Al vencer el plazo, la IP llena vuelve a tener lugar.
    sleep(ctl::PLAZO_AUTH + Duration::from_millis(50)).await;
    colgadas.clear();
    for _ in 0..ctl::MAX_SIN_AUTH_POR_IP {
        colgadas.push(handshake_desde(&b, local(), Some("198.51.100.7")).await.expect("plazo vencido: lugar libre"));
    }
    assert_eq!(handshake_desde(&b, local(), Some("198.51.100.7")).await.err(), Some(503));
}

#[tokio::test(start_paused = true)]
async fn ctl_las_ipv6_de_un_mismo_64_cuentan_como_una() {
    // Con IPv6 una sola conexión de casa tiene un /64 entero: contar cada dirección no pondría tope.
    let b = banco();
    let mut colgadas = Vec::new();
    for i in 0..ctl::MAX_SIN_AUTH_POR_IP {
        let ip = format!("2001:db8:1:2::{:x}", i + 1);
        colgadas.push(handshake_desde(&b, local(), Some(&ip)).await.expect("dentro del tope del /64"));
    }
    assert_eq!(handshake_desde(&b, local(), Some("2001:db8:1:2:ffff::1")).await.err(), Some(503));
    assert!(handshake_desde(&b, local(), Some("2001:db8:1:3::1")).await.is_ok(), "otro /64 entra");
}

#[tokio::test(start_paused = true)]
async fn ctl_si_el_par_no_es_local_el_x_forwarded_for_no_cuenta() {
    // Solo Caddy (desde la misma máquina) puede decir de qué IP viene la conexión.
    let b = banco();
    let mut colgadas = Vec::new();
    for i in 0..ctl::MAX_SIN_AUTH_POR_IP {
        colgadas.push(handshake_desde(&b, remota(), Some(&ip_n(i))).await.expect("dentro del tope de la IP"));
    }
    assert_eq!(handshake_desde(&b, remota(), Some(&ip_n(99))).await.err(), Some(503), "cuenta la IP del par");
    assert!(handshake_desde(&b, local(), Some(&ip_n(99))).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn ctl_los_lugares_se_liberan_al_cerrar_antes_de_auth_y_al_autenticarse() {
    let b = banco();
    let ip = "198.51.100.7";
    let tope = ctl::MAX_SIN_AUTH_POR_IP;
    // Cerrar antes de `auth`: con cierre de WebSocket y cortando la conexión sin más.
    for i in 0..tope {
        let mut ws = handshake_desde(&b, local(), Some(ip)).await.expect("dentro del tope");
        if i % 2 == 0 {
            ws.close(None).await.unwrap();
        }
        drop(ws);
    }
    // Mucho menos que el plazo de auth: lo libera el cierre, no el plazo.
    sleep(Duration::from_millis(10)).await;
    assert_eq!(b.estado.sin_auth.ocupados(), (0, 0), "ni lugar global ni entrada de la IP");
    // Autenticarse también libera el lugar (bien o con una credencial mala).
    let mut abiertas = Vec::new();
    for i in 0..tope {
        let mut ws = handshake_desde(&b, local(), Some(ip)).await.expect("los cerrados liberaron el lugar");
        let tk = if i == 0 { "basura".to_string() } else { token("operador") };
        enviar(&mut ws, json!({"t":"auth","token":tk})).await;
        assert!(recibir(&mut ws).await.is_some());
        abiertas.push(ws);
    }
    let mut colgadas = Vec::new();
    for _ in 0..tope {
        colgadas.push(handshake_desde(&b, local(), Some(ip)).await.expect("las autenticadas no ocupan lugar"));
    }
    assert_eq!(handshake_desde(&b, local(), Some(ip)).await.err(), Some(503));
    assert_eq!(b.estado.sin_auth.ocupados(), (tope, 1));
}

#[test]
fn ctl_los_lugares_sin_auth_se_devuelven_al_soltarlos() {
    // Un upgrade fallido suelta la clausura (y el lugar) sin llamarla: alcanza con soltarlo.
    let s = ctl::SinAuth::new();
    let a: std::net::IpAddr = "198.51.100.7".parse().unwrap();
    let otra: std::net::IpAddr = "198.51.100.8".parse().unwrap();
    let lugares: Vec<_> = (0..ctl::MAX_SIN_AUTH_POR_IP).map(|_| s.tomar(a).unwrap()).collect();
    assert_eq!(s.tomar(a).err(), Some(ctl::SinLugar::PorIp));
    let b = s.tomar(otra).unwrap();
    assert_eq!(s.ocupados(), (ctl::MAX_SIN_AUTH_POR_IP + 1, 2));
    drop(lugares);
    assert_eq!(s.ocupados(), (1, 1));
    drop(b);
    assert_eq!(s.ocupados(), (0, 0));
    // Un rechazo no deja nada tomado.
    let todos: Vec<_> = (0..ctl::MAX_SIN_AUTH).map(|i| s.tomar(ip_n(i / ctl::MAX_SIN_AUTH_POR_IP).parse().unwrap()).unwrap()).collect();
    assert_eq!(s.tomar("192.0.2.1".parse().unwrap()).err(), Some(ctl::SinLugar::Global));
    assert_eq!(s.ocupados(), (ctl::MAX_SIN_AUTH, ctl::MAX_SIN_AUTH / ctl::MAX_SIN_AUTH_POR_IP));
    drop(todos);
    assert_eq!(s.ocupados(), (0, 0));
}

#[test]
fn ctl_la_ip_del_cliente_sale_del_x_forwarded_for_solo_si_el_par_es_local() {
    let h = |x: &str| {
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-forwarded-for", x.parse().unwrap());
        h
    };
    let ip = |t: &str| -> std::net::IpAddr { t.parse().unwrap() };
    let sin = axum::http::HeaderMap::new();
    assert_eq!(ctl::ip_cliente(local(), &h("198.51.100.7")), ip("198.51.100.7"));
    assert_eq!(ctl::ip_cliente(local(), &h("192.0.2.1, 198.51.100.7")), ip("198.51.100.7"), "la última, la de Caddy");
    assert_eq!(ctl::ip_cliente(local(), &h("basura")), ip("127.0.0.1"));
    assert_eq!(ctl::ip_cliente(local(), &sin), ip("127.0.0.1"));
    assert_eq!(ctl::ip_cliente(local(), &h("::ffff:198.51.100.7")), ip("198.51.100.7"));
    assert_eq!(ctl::ip_cliente("[::1]:5000".parse().unwrap(), &h("2001:db8::1")), ip("2001:db8::1"));
    assert_eq!(ctl::ip_cliente(remota(), &h("198.51.100.7")), remota().ip(), "de afuera no se cree");
    assert_eq!(ctl::clave_ip(ip("2001:db8:1:2:3:4:5:6")), ip("2001:db8:1:2::"));
    assert_eq!(ctl::clave_ip(ip("198.51.100.7")), ip("198.51.100.7"));
}

#[tokio::test(start_paused = true)]
async fn ctl_una_credencial_mas_larga_que_las_de_la_api_se_corta_a_la_hora_y_30_s() {
    // Las credenciales de la api duran 1 h: un `exp` más largo (un error de la api) no da más control.
    assert_eq!(ctl::TOPE_CONEXION, Duration::from_secs(3600 + 30));
    let b = banco();
    let mut c = claims("operador");
    c["exp"] = json!(AHORA + 2 * 3600);
    let (mut ws, r) = b.ws_con(&firmar(&c)).await;
    assert_eq!(r, json!({"t":"ok","rol":"operador"}));
    let t = Instant::now();
    assert_eq!(recibir(&mut ws).await, Some(json!({"t":"error","codigo":"VENCIDA"})));
    assert!(t.elapsed() <= Duration::from_secs(3630), "{:?}", t.elapsed());
}

#[tokio::test(start_paused = true)]
async fn ctl_al_terminar_cuenta_los_descartados_del_filtro() {
    let b = banco_con_turno().await;
    let (mut ws, _) = b.ws_con(&token("jugador")).await;
    // Tres toques de PS y uno de Create: los cuatro descartados.
    for botones in [PS, 0, PS, 0, PS, 0, CREATE, 0] {
        enviar(&mut ws, estado_json(botones)).await;
    }
    sleep(Duration::from_millis(5)).await;
    ws.close(None).await.unwrap();
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.estado.descartados(), 4);
    // El operador no descarta nada: el total no cambia.
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws, estado_json(PS)).await;
    ws.close(None).await.unwrap();
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.estado.descartados(), 4);
}

// ---------------------------------------------------------------- /interno

fn auth(action: &str, protocol: &str, ip: &str, path: &str, token: &str) -> Value {
    json!({"user":"","password":"","token":token,"ip":ip,"action":action,"path":path,
           "protocol":protocol,"id":null,"query":""})
}

#[tokio::test(start_paused = true)]
async fn interno_auth_tabla_de_casos() {
    let b = banco();
    let op = token("operador");
    let mut vencido = claims("operador");
    vencido["exp"] = json!(AHORA - 31);
    let mut otra_aud = claims("operador");
    otra_aud["aud"] = json!("otra");
    let casos: Vec<(Value, StatusCode)> = vec![
        (auth("publish", "rtsp", "127.0.0.1", "ps5", ""), StatusCode::OK),
        (auth("read", "rtsp", "127.0.0.1", "ps5", ""), StatusCode::OK),
        (auth("publish", "udp", "127.0.0.1", "ps5", ""), StatusCode::OK),
        (auth("read", "udp", "127.0.0.1", "ps5", ""), StatusCode::OK),
        (auth("read", "rtsp", "::1", "ps5", ""), StatusCode::OK),
        (auth("read", "webrtc", "198.51.100.7", "ps5", &op), StatusCode::OK),
        // Todo lo demás, 401.
        (auth("publish", "rtsp", "198.51.100.7", "ps5", ""), StatusCode::UNAUTHORIZED),
        (auth("read", "rtsp", "198.51.100.7", "ps5", &op), StatusCode::UNAUTHORIZED),
        (auth("read", "rtsp", "", "ps5", ""), StatusCode::UNAUTHORIZED),
        (auth("publish", "webrtc", "127.0.0.1", "ps5", &op), StatusCode::UNAUTHORIZED),
        (auth("read", "webrtc", "198.51.100.7", "ps5", ""), StatusCode::UNAUTHORIZED),
        (auth("read", "webrtc", "198.51.100.7", "otra", &op), StatusCode::UNAUTHORIZED),
        (auth("read", "webrtc", "198.51.100.7", "ps5", &token("jugador")), StatusCode::UNAUTHORIZED),
        (auth("read", "webrtc", "198.51.100.7", "ps5", &token("vista")), StatusCode::UNAUTHORIZED),
        (auth("read", "webrtc", "198.51.100.7", "ps5", &firmar(&vencido)), StatusCode::UNAUTHORIZED),
        (auth("read", "webrtc", "198.51.100.7", "ps5", &firmar(&otra_aud)), StatusCode::UNAUTHORIZED),
        (auth("read", "webrtc", "198.51.100.7", "ps5", "basura"), StatusCode::UNAUTHORIZED),
        (auth("read", "hls", "198.51.100.7", "ps5", &op), StatusCode::UNAUTHORIZED),
        (auth("publish", "rtmp", "127.0.0.1", "ps5", ""), StatusCode::UNAUTHORIZED),
        (auth("publish", "srt", "127.0.0.1", "ps5", ""), StatusCode::UNAUTHORIZED),
        // La API de MediaMTX (solo la usa miturno-rp para expulsar sesiones WHEP): solo local.
        (auth("api", "", "127.0.0.1", "", ""), StatusCode::OK),
        (auth("api", "", "::1", "", ""), StatusCode::OK),
        (auth("api", "", "198.51.100.7", "", ""), StatusCode::UNAUTHORIZED),
        (auth("api", "", "", "", ""), StatusCode::UNAUTHORIZED),
        (auth("metrics", "", "127.0.0.1", "", ""), StatusCode::UNAUTHORIZED),
        (auth("playback", "", "127.0.0.1", "ps5", &op), StatusCode::UNAUTHORIZED),
        (auth("pprof", "", "127.0.0.1", "", ""), StatusCode::UNAUTHORIZED),
    ];
    for (cuerpo, esperado) in casos {
        assert_eq!(b.auth_mediamtx(cuerpo.clone()).await, esperado, "{cuerpo}");
    }
}

fn whep(token: &str, id: Option<&str>) -> Value {
    let mut c = auth("read", "webrtc", "198.51.100.7", "ps5", token);
    c["id"] = json!(id);
    c
}

#[tokio::test(start_paused = true)]
async fn interno_auth_whep_segun_el_turno_vigente_y_las_vistas() {
    let b = banco();
    b.empujar_con(1, Some(("u1", "t1")), &["u2"]).await;
    let id = uuid(1);
    let casos: Vec<(&str, String, StatusCode)> = vec![
        ("jugador del turno vigente", token_jugador("u1", "t1"), StatusCode::OK),
        ("vista en vistas", token_vista("u2"), StatusCode::OK),
        ("vista que no está", token_vista("u3"), StatusCode::UNAUTHORIZED),
        ("jugador de otro turno y otra cuenta", token_jugador("u9", "t0"), StatusCode::UNAUTHORIZED),
        ("misma cuenta que el vigente (la vista que pasó a jugar)", token_jugador("u1", "t0"), StatusCode::OK),
        ("operador", token_operador("op"), StatusCode::OK),
    ];
    for (que, t, esperado) in casos {
        assert_eq!(b.ver(&t, Some(&id)).await, esperado, "{que}");
        assert_eq!(b.ver(&t, None).await, esperado, "{que}, sin id");
    }
    let mut otra_ruta = whep(&token_jugador("u1", "t1"), Some(&id));
    otra_ruta["path"] = json!("kick-humo");
    assert_eq!(b.auth_mediamtx(otra_ruta).await, StatusCode::UNAUTHORIZED, "solo la ruta ps5");
    let mut vencido = claims("vista");
    vencido["sub"] = json!("u2");
    vencido["exp"] = json!(AHORA - 31);
    assert_eq!(b.ver(&firmar(&vencido), Some(&id)).await, StatusCode::UNAUTHORIZED, "credencial vencida");
    // Sin empuje (como tras un reinicio) solo pasa el operador.
    let b = banco();
    assert_eq!(b.ver(&token_jugador("u1", "t1"), None).await, StatusCode::UNAUTHORIZED);
    assert_eq!(b.ver(&token_vista("u2"), None).await, StatusCode::UNAUTHORIZED);
    assert_eq!(b.ver(&token_operador("op"), None).await, StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn whep_al_cambiar_el_turno_se_expulsa_a_quien_ya_no_tiene_video() {
    // Review Focus 4: MediaMTX no vuelve a validar una sesión abierta.
    let b = banco();
    b.empujar_con(1, Some(("u1", "t1")), &["u2"]).await;
    let (s1, s2, op) = (uuid(1), uuid(2), uuid(3));
    assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&s1)).await, StatusCode::OK);
    assert_eq!(b.ver(&token_vista("u2"), Some(&s2)).await, StatusCode::OK);
    assert_eq!(b.ver(&token_operador("op"), Some(&op)).await, StatusCode::OK);
    assert!(b.expulsiones().await.is_empty(), "abrir una sesión de otra cuenta no expulsa a nadie");
    // u2 pasa a jugar (con su credencial de vista todavía abierta): conserva el video; u1 lo pierde.
    b.empujar_con(2, Some(("u2", "t2")), &["u3"]).await;
    assert_eq!(b.expulsiones().await, vec![s1.clone()]);
    // Termina el turno y no queda nadie: se va u2. La del operador, nunca.
    b.empujar_con(3, None, &[]).await;
    assert_eq!(b.expulsiones().await, vec![s1.clone(), s2.clone()], "u1 ya se expulsó: no se repite");
    b.empujar_con(4, None, &[]).await;
    assert_eq!(b.expulsiones().await, vec![s1, s2]);
}

#[tokio::test(start_paused = true)]
async fn whep_una_cuenta_tiene_una_sola_sesion_y_el_operador_varias() {
    let b = banco();
    b.empujar_con(1, Some(("u1", "t1")), &["u2"]).await;
    let (a, c) = (uuid(1), uuid(2));
    assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&a)).await, StatusCode::OK);
    assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&c)).await, StatusCode::OK, "otra pestaña");
    assert_eq!(b.expulsiones().await, vec![a.clone()], "la nueva expulsa a la anterior");
    // El mismo id otra vez (MediaMTX no lo hace, pero por las dudas) no se expulsa a sí mismo.
    assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&c)).await, StatusCode::OK);
    assert_eq!(b.expulsiones().await, vec![a.clone()]);
    // La vista que pasa a jugar y abre otra sesión con la credencial nueva: se va la de vista.
    let (v, j) = (uuid(3), uuid(4));
    assert_eq!(b.ver(&token_vista("u2"), Some(&v)).await, StatusCode::OK);
    b.empujar_con(2, Some(("u2", "t2")), &[]).await;
    assert_eq!(b.ver(&token_jugador("u2", "t2"), Some(&j)).await, StatusCode::OK);
    let exp = b.expulsiones().await;
    assert!(exp.contains(&v) && !exp.contains(&j), "{exp:?}");
    // Dos del operador conviven, y ningún empuje las toca.
    let (o1, o2) = (uuid(5), uuid(6));
    let antes = b.expulsiones().await.len();
    assert_eq!(b.ver(&token_operador("op"), Some(&o1)).await, StatusCode::OK);
    assert_eq!(b.ver(&token_operador("op"), Some(&o2)).await, StatusCode::OK);
    b.empujar_con(3, None, &[]).await;
    let exp = b.expulsiones().await;
    assert!(!exp.contains(&o1) && !exp.contains(&o2), "{exp:?}");
    assert!(exp.len() > antes, "el empuje sí expulsa a los demás: {exp:?}");
}

#[tokio::test(start_paused = true)]
async fn whep_un_id_que_no_es_uuid_se_autoriza_pero_no_se_registra() {
    let b = banco();
    b.empujar_con(1, Some(("u1", "t1")), &[]).await;
    let t = token_jugador("u1", "t1");
    let raros = [
        "x; rm -rf /",
        "../../v3/config/global/patch",
        "00000001-0000-4000-8000-00000000000G",
        "00000001-0000-4000-8000-00000000000A",
        "00000001-0000-4000-8000-0000000000001",
        "000000010000-4000-8000-000000000001",
        "",
    ];
    for id in raros {
        assert_eq!(b.ver(&t, Some(id)).await, StatusCode::OK, "{id:?}");
    }
    assert_eq!(b.ver(&t, None).await, StatusCode::OK, "id null (como en un OPTIONS)");
    let bueno = uuid(1);
    assert_eq!(b.ver(&t, Some(&bueno)).await, StatusCode::OK);
    assert!(b.expulsiones().await.is_empty(), "nada raro se registró como sesión anterior");
    b.empujar_con(2, None, &[]).await;
    assert_eq!(b.expulsiones().await, vec![bueno], "solo el UUID llega a expulsar_whep");
}

#[tokio::test(start_paused = true)]
async fn whep_un_kick_fallido_se_reintenta_a_1_2_y_4_s() {
    let b = banco();
    b.empujar_con(1, Some(("u1", "t1")), &[]).await;
    let s = uuid(1);
    assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&s)).await, StatusCode::OK);
    *b.sistema.fallas.lock().unwrap() = 2;
    let t0 = Instant::now();
    b.empujar_con(2, None, &[]).await;
    sleep(Duration::from_secs(20)).await;
    assert_eq!(b.momentos_ms(t0), vec![0, 1_000, 3_000], "la tercera sale bien");
    b.empujar_con(3, None, &[]).await;
    assert_eq!(b.expulsiones().await.len(), 3, "expulsada: ya no se intenta");
}

#[tokio::test(start_paused = true)]
async fn whep_si_fallan_los_4_intentos_se_reintenta_en_el_proximo_empuje() {
    let b = banco();
    b.empujar_con(1, Some(("u1", "t1")), &[]).await;
    let s = uuid(1);
    assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&s)).await, StatusCode::OK);
    *b.sistema.fallas.lock().unwrap() = 4;
    let t0 = Instant::now();
    b.empujar_con(2, None, &[]).await;
    sleep(Duration::from_secs(60)).await;
    assert_eq!(b.momentos_ms(t0), vec![0, 1_000, 3_000, 7_000], "y después se deja para el próximo empuje");
    b.empujar_con(3, None, &[]).await;
    assert_eq!(b.expulsiones().await, vec![s.clone(); 5], "sigue registrada: se reintenta");
    b.empujar_con(4, None, &[]).await;
    assert_eq!(b.expulsiones().await, vec![s; 5], "expulsada: ya no se intenta");
}

#[tokio::test(start_paused = true)]
async fn whep_muchas_pestanas_a_la_vez_no_multiplican_los_curl() {
    // Cada WHEP de la misma cuenta pedía de nuevo las expulsiones que ya estaban en vuelo: N pestañas
    // eran ~N²/2 procesos de curl. Cada sesión se expulsa una vez, y de a una.
    let b = banco();
    b.empujar_con(1, Some(("u1", "t1")), &[]).await;
    *b.sistema.demora.lock().unwrap() = Duration::from_secs(2);
    for n in 1..=8 {
        assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&uuid(n))).await, StatusCode::OK);
    }
    sleep(Duration::from_secs(60)).await;
    let mut ids = b.expulsiones().await;
    ids.sort();
    assert_eq!(ids, (1..=7).map(uuid).collect::<Vec<_>>(), "cada anterior, una sola vez");
    assert_eq!(b.sistema.en_vuelo.lock().unwrap().1, 1, "un solo curl a la vez");
    // Un empuje en medio de expulsiones lentas tampoco repite las que están en vuelo.
    for n in 9..=10 {
        assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&uuid(n))).await, StatusCode::OK);
    }
    b.empujar_con(2, None, &[]).await;
    b.empujar_con(3, None, &[]).await;
    sleep(Duration::from_secs(60)).await;
    let mut ids = b.expulsiones().await;
    ids.sort();
    assert_eq!(ids, (1..=10).map(uuid).collect::<Vec<_>>());
}

#[tokio::test(start_paused = true)]
async fn turno_responde_sin_esperar_a_la_expulsion() {
    let b = banco();
    b.empujar_con(1, Some(("u1", "t1")), &[]).await;
    assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&uuid(1))).await, StatusCode::OK);
    *b.sistema.demora.lock().unwrap() = Duration::from_secs(30);
    let t = Instant::now();
    b.empujar_con(2, None, &[]).await;
    assert!(t.elapsed() < Duration::from_millis(100), "{:?}", t.elapsed());
    assert_eq!(b.expulsiones().await, vec![uuid(1)], "la expulsión ya arrancó, en segundo plano");
}

/// Ruta para el registro de sesiones de un test (vacía: sin archivo).
fn ruta_whep(nombre: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("miturno-rp-servidor-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(nombre);
    let _ = std::fs::remove_file(&p);
    p
}

#[tokio::test(start_paused = true)]
async fn whep_tras_un_reinicio_se_expulsan_las_que_ya_no_corresponden() {
    let ruta = ruta_whep("whep-reinicio");
    let b = banco_con(Some(ruta.clone()));
    b.empujar_con(1, Some(("u1", "t1")), &["u2"]).await;
    let (a, v, op) = (uuid(1), uuid(2), uuid(3));
    assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&a)).await, StatusCode::OK);
    assert_eq!(b.ver(&token_vista("u2"), Some(&v)).await, StatusCode::OK);
    assert_eq!(b.ver(&token_operador("op"), Some(&op)).await, StatusCode::OK);
    sleep(Duration::from_millis(50)).await;
    let guardado = std::fs::read_to_string(&ruta).unwrap();
    assert!(guardado.contains(&a) && guardado.contains(&v) && !guardado.contains(&op), "{guardado}");
    drop(b);

    // Reinicio: hasta el primer empuje no se sabe quién corresponde; no se expulsa a nadie.
    let b = banco_con(Some(ruta.clone()));
    sleep(Duration::from_secs(5)).await;
    assert!(b.expulsiones().await.is_empty());
    // u2 pasó a jugar con su credencial de vista abierta: la conserva. u1 ya no.
    b.empujar_con(2, Some(("u2", "t2")), &[]).await;
    assert_eq!(b.expulsiones().await, vec![a.clone()]);
    sleep(Duration::from_millis(50)).await;
    let guardado = std::fs::read_to_string(&ruta).unwrap();
    assert!(!guardado.contains(&a) && guardado.contains(&v), "la expulsada sale del disco: {guardado}");
    drop(b);

    let b = banco_con(Some(ruta.clone()));
    b.empujar_con(3, None, &[]).await;
    assert_eq!(b.expulsiones().await, vec![v], "la del operador nunca");
    let _ = std::fs::remove_file(&ruta);
}

#[tokio::test(start_paused = true)]
async fn whep_un_registro_corrupto_se_descarta_y_se_arranca_vacio() {
    let ruta = ruta_whep("whep-corrupto");
    std::fs::write(&ruta, "esto no es el registro").unwrap();
    let b = banco_con(Some(ruta.clone()));
    assert!(!ruta.exists(), "el corrupto se borra");
    b.empujar_con(1, Some(("u1", "t1")), &[]).await;
    assert!(b.expulsiones().await.is_empty());
    assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&uuid(1))).await, StatusCode::OK);
    sleep(Duration::from_millis(50)).await;
    assert!(std::fs::read_to_string(&ruta).unwrap().contains(&uuid(1)), "se vuelve a guardar");
    let _ = std::fs::remove_file(&ruta);
}

#[tokio::test(start_paused = true)]
async fn whep_un_empuje_con_version_vieja_no_expulsa_a_nadie() {
    let b = banco();
    b.empujar_con(5, Some(("u1", "t1")), &[]).await;
    assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&uuid(1))).await, StatusCode::OK);
    let (st, _) = b.turno(&cuerpo_autorizados(4, None, &[])).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(b.expulsiones().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn interno_auth_tolera_campos_nuevos_y_rechaza_cuerpos_rotos() {
    let b = banco();
    let mut c = auth("read", "rtsp", "127.0.0.1", "ps5", "");
    c["campoNuevo"] = json!(1);
    assert_eq!(b.auth_mediamtx(c).await, StatusCode::OK, "un campo nuevo de MediaMTX no rompe nada");
    for cuerpo in ["", "no es json", "[]", r#"{"action":1}"#] {
        let (st, _) = b.pedir("POST", "/interno/auth", local(), None, cuerpo).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{cuerpo:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn interno_solo_desde_localhost() {
    let b = banco();
    let cuerpo = auth("read", "rtsp", "127.0.0.1", "ps5", "").to_string();
    for ruta in ["/interno/keyframe", "/interno/auth"] {
        let (st, _) = b.pedir("POST", ruta, remota(), None, &cuerpo).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{ruta}");
    }
    assert_eq!(b.sesion.interna().registro().keyframes, 0);
}

#[tokio::test(start_paused = true)]
async fn interno_rechaza_pedidos_que_vienen_por_el_proxy() {
    let b = banco();
    let r = rutas(b.estado.clone())
        .layer(MockConnectInfo(local()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/interno/keyframe")
                .header("x-forwarded-for", "198.51.100.7")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "Caddy agrega X-Forwarded-For: no es local");
}

#[tokio::test(start_paused = true)]
async fn interno_keyframe_desde_localhost_pasa_por_el_limitador() {
    let b = banco();
    let (st, cuerpo) = b.pedir("POST", "/interno/keyframe", local(), None, "").await;
    assert_eq!((st, cuerpo.as_str()), (StatusCode::OK, r#"{"enviado":true}"#));
    let (_, cuerpo) = b.pedir("POST", "/interno/keyframe", local(), None, "").await;
    assert_eq!(cuerpo, r#"{"enviado":false}"#);
    assert_eq!(b.sesion.interna().registro().keyframes, 1);
}

// ---------------------------------------------------------------- /control

const ACCIONES: [(&str, &str); 6] = [
    ("GET", "salud"),
    ("POST", "reconectar"),
    ("POST", "kick_on"),
    ("POST", "kick_off"),
    ("POST", "reposo"),
    ("POST", "encender"),
];

#[tokio::test(start_paused = true)]
async fn control_sin_token_o_con_token_malo_es_401() {
    let b = banco();
    let malos = [
        None,
        Some("Bearer ".to_string()),
        Some("Bearer otro".to_string()),
        Some(format!("Bearer {TOKEN_CONTROL}x")),
        Some(format!("Bearer {}", &TOKEN_CONTROL[..TOKEN_CONTROL.len() - 1])),
        Some(format!("Basic {TOKEN_CONTROL}")),
        Some(TOKEN_CONTROL.to_string()),
        // La credencial del navegador no sirve para el control.
        Some(format!("Bearer {}", token("operador"))),
    ];
    for (metodo, accion) in ACCIONES {
        for auth in &malos {
            let (st, _) = b.pedir(metodo, &format!("/control/{accion}"), local(), auth.as_deref(), "").await;
            assert_eq!(st, StatusCode::UNAUTHORIZED, "{metodo} {accion} con {auth:?}");
        }
    }
    assert!(b.sistema.llamadas.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn control_con_el_token_bueno_es_200() {
    let mut b = banco();
    for (metodo, accion) in ACCIONES {
        let (st, cuerpo) = b.control(metodo, accion).await;
        assert_eq!(st, StatusCode::OK, "{metodo} {accion}: {cuerpo}");
    }
    assert_eq!(
        *b.sistema.llamadas.lock().unwrap(),
        vec![("start".into(), "miturno-kick-out.service".into()), ("stop".into(), "miturno-kick-out.service".into())]
    );
    let mut ordenes = Vec::new();
    while let Ok(o) = b.ordenes.try_recv() {
        ordenes.push(o);
    }
    assert_eq!(ordenes, vec![Orden::Reconectar, Orden::Reposo, Orden::Encender]);
}

#[tokio::test(start_paused = true)]
async fn control_el_token_vale_aunque_venga_por_el_proxy() {
    let b = banco();
    let (st, _) = b.pedir("GET", "/control/salud", remota(), Some(&format!("Bearer {TOKEN_CONTROL}")), "").await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn control_kick_con_systemctl_fallando_es_502() {
    let b = banco();
    *b.sistema.falla_systemctl.lock().unwrap() = true;
    let (st, cuerpo) = b.control("POST", "kick_on").await;
    assert_eq!(st, StatusCode::BAD_GATEWAY);
    assert_eq!(cuerpo, r#"{"codigo":"SYSTEMCTL"}"#);
}

#[tokio::test(start_paused = true)]
async fn control_orden_sin_ciclo_es_503() {
    let mut b = banco();
    b.ordenes.close();
    let bearer = format!("Bearer {TOKEN_CONTROL}");
    let (st, cuerpo) = b.pedir("POST", "/control/reposo", local(), Some(&bearer), "").await;
    assert_eq!((st, cuerpo.as_str()), (StatusCode::SERVICE_UNAVAILABLE, r#"{"codigo":"CICLO"}"#));
}

#[tokio::test(start_paused = true)]
async fn salud_tiene_la_forma_de_video_salud() {
    let b = banco();
    {
        let mut e = b.sistema.estados.lock().unwrap();
        e.insert("miturno-kick-out.service".into(), "active".into());
        e.insert("mediamtx.service".into(), "active".into());
        e.insert("coturn.service".into(), "inactive".into());
    }
    let t0 = Instant::now();
    b.metricas.conectada(t0, std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(AHORA));
    b.metricas.cuadro(t0, 12_500, 2);
    let (st, cuerpo) = b.control("GET", "salud").await;
    assert_eq!(st, StatusCode::OK);
    let v: Value = serde_json::from_str(&cuerpo).unwrap();
    assert_eq!(
        v,
        json!({
            "sesion": {"estado":"conectada","fps":1.0,"kbps":100,"perdidos":2,"ultimaReconexion":"2027-01-15T08:00:00Z"},
            "kick": {"encendido":true,"estado":"transmitiendo"},
            "mediamtx": true,
            "turn": false,
            "cpu": 0.0,
            "jugador": {"version":null,"turnId":null,"mando":null,"inactivoMs":null}
        })
    );
}

#[tokio::test(start_paused = true)]
async fn salud_informa_reposo() {
    let b = banco();
    b.metricas.estado(crate::metricas::EstadoSesion::Reposo);
    let (_, cuerpo) = b.control("GET", "salud").await;
    let v: Value = serde_json::from_str(&cuerpo).unwrap();
    assert_eq!(v["sesion"], json!({"estado":"reposo","fps":0.0,"kbps":0,"perdidos":0,"ultimaReconexion":null}));
}

#[test]
fn estado_de_kick_segun_systemd() {
    let casos = [
        ("active", true, "transmitiendo"),
        ("reloading", true, "transmitiendo"),
        ("activating", true, "error"),
        ("inactive", false, "apagado"),
        ("deactivating", false, "apagado"),
        ("failed", false, "error"),
        ("desconocido", false, "error"),
    ];
    for (activo, encendido, estado) in casos {
        let k = estado_kick(activo);
        assert_eq!((k.encendido, k.estado), (encendido, estado), "{activo}");
    }
}

#[tokio::test(start_paused = true)]
async fn salud_informa_la_cpu_muestreada() {
    let b = banco();
    b.sistema.cpu.lock().unwrap().extend([
        TiemposCpu { total: 1_000, ociosa: 900 },
        TiemposCpu { total: 2_000, ociosa: 1_500 },
    ]);
    let tarea = tokio::spawn(muestrear_cpu(b.estado.clone()));
    sleep(INTERVALO_CPU + Duration::from_millis(10)).await;
    let (_, cuerpo) = b.control("GET", "salud").await;
    let v: Value = serde_json::from_str(&cuerpo).unwrap();
    assert_eq!(v["cpu"], json!(40.0));
    tarea.abort();
}

// ---------------------------------------------------------------- arranque

fn archivo(nombre: &str, contenido: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("miturno-rp-servidor-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(nombre);
    std::fs::write(&p, contenido).unwrap();
    p
}

#[test]
fn la_clave_publica_se_valida_al_arrancar() {
    let buena = archivo("buena.pem", &par(1).1);
    assert!(cargar_clave_publica(&buena).is_ok());
    let e = cargar_clave_publica(std::path::Path::new("/no/existe.pem")).err().unwrap();
    assert!(e.contains("/no/existe.pem") && e.contains("no existe"), "{e}");
    let privada = par(1).0;
    let mala = archivo("mala.pem", &privada);
    let e = cargar_clave_publica(&mala).err().unwrap();
    assert!(e.contains("mala.pem") && e.contains("Ed25519"), "{e}");
    assert!(!e.contains("PRIVATE"), "el error no muestra el contenido: {e}");
}

#[test]
fn el_token_de_control_se_lee_una_vez_y_no_se_muestra() {
    let p = archivo("control.token", &format!("{TOKEN_CONTROL}\n"));
    let t = cargar_token_control(&p).unwrap();
    assert!(t.coincide(TOKEN_CONTROL.as_bytes()), "se recorta el salto de línea");
    assert!(!format!("{t:?}").contains(TOKEN_CONTROL));
    let corto = archivo("corto.token", "abc123");
    let e = cargar_token_control(&corto).err().unwrap();
    assert!(e.contains("corto.token") && !e.contains("abc123"), "{e}");
    assert!(cargar_token_control(&archivo("vacio.token", "\n")).is_err());
    assert!(cargar_token_control(std::path::Path::new("/no/existe.token")).is_err());
}

// ---------------------------------------------------------------- /control/turno

#[tokio::test(start_paused = true)]
async fn turno_sin_token_o_con_token_malo_es_401() {
    let b = banco();
    let cuerpo = cuerpo_turno(1, Some(("u1", "t1")));
    let malos = [None, Some("Bearer otro".to_string()), Some(format!("Bearer {}", token("operador")))];
    for auth in &malos {
        let (st, r) = b.pedir("POST", "/control/turno", local(), auth.as_deref(), &cuerpo).await;
        assert_eq!((st, r.as_str()), (StatusCode::UNAUTHORIZED, r#"{"codigo":"TOKEN"}"#), "{auth:?}");
    }
    assert_eq!(b.salud_jugador().await["version"], Value::Null, "nada se guardó");
}

#[tokio::test(start_paused = true)]
async fn turno_con_cuerpo_invalido_es_400_formato() {
    let b = banco();
    let tres = r#"{"version":1,"jugador":null,"vistas":["a","b","c"]}"#;
    let vacio = r#"{"version":1,"jugador":{"sub":"","turnId":"t1"},"vistas":[]}"#;
    for cuerpo in ["", "no es json", "[]", r#"{"version":1,"vistas":[]}"#, tres, vacio] {
        let (st, r) = b.turno(cuerpo).await;
        assert_eq!((st, r.as_str()), (StatusCode::BAD_REQUEST, r#"{"codigo":"FORMATO"}"#), "{cuerpo:?}");
    }
    assert_eq!(b.salud_jugador().await["version"], Value::Null);
}

#[tokio::test(start_paused = true)]
async fn turno_version_menor_es_409_e_igual_es_idempotente() {
    let b = banco();
    b.empujar(5, Some(("u1", "t1"))).await;
    b.empujar(5, Some(("u1", "t1"))).await;
    let (st, r) = b.turno(&cuerpo_turno(4, None)).await;
    assert_eq!((st, r.as_str()), (StatusCode::CONFLICT, r#"{"codigo":"VERSION"}"#));
    let j = b.salud_jugador().await;
    assert_eq!((j["version"].clone(), j["turnId"].clone()), (json!(5), json!("t1")), "el 409 no cambió nada");
}

#[tokio::test(start_paused = true)]
async fn ctl_jugador_sin_su_turno_vigente_es_turno_sin_tocar_la_sesion() {
    let b = banco();
    let (mut ws, r) = b.ws_con(&token_jugador("u1", "t1")).await;
    assert_eq!(r, json!({"t":"error","codigo":"TURNO"}));
    assert!(cerrado(&mut ws).await);
    assert!(b.mandos().is_empty(), "la sesión no recibió nada: {:?}", b.mandos());
    b.empujar(1, Some(("u1", "t1"))).await;
    // Otro turno de la misma cuenta, y la misma credencial con otra cuenta: tampoco.
    let (_, r) = b.ws_con(&token_jugador("u1", "t0")).await;
    assert_eq!(r, json!({"t":"error","codigo":"TURNO"}));
    let (_, r) = b.ws_con(&token_jugador("u2", "t1")).await;
    assert_eq!(r, json!({"t":"error","codigo":"TURNO"}));
    assert!(b.mandos().is_empty());
    let (_ws, r) = b.ws_con(&token_jugador("u1", "t1")).await;
    assert_eq!(r, json!({"t":"ok","rol":"jugador"}));
}

#[tokio::test(start_paused = true)]
async fn turno_que_termina_corta_al_jugador_antes_de_responder() {
    let b = banco_con_turno().await;
    let (mut ws, _) = b.ws_con(&token_jugador("u1", "t1")).await;
    enviar(&mut ws, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(CRUZ));
    let t = Instant::now();
    b.empujar(2, None).await;
    // Cuando llega el 200, la PS5 ya recibió el todo suelto.
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto());
    assert_eq!(recibir(&mut ws).await, Some(json!({"t":"error","codigo":"TURNO"})));
    assert!(t.elapsed() <= Duration::from_millis(50), "{:?}", t.elapsed());
    assert!(cerrado(&mut ws).await);
    let m = b.mandos();
    let i = m.iter().rposition(|e| *e == con(CRUZ)).unwrap();
    assert_eq!(&m[i + 1..], &[EstadoMando::suelto()], "suelto exactamente una vez: {m:?}");
    // La ranura quedó libre: el jugador del turno siguiente entra.
    b.empujar(3, Some(("u2", "t2"))).await;
    let (_ws2, r) = b.ws_con(&token_jugador("u2", "t2")).await;
    assert_eq!(r, json!({"t":"ok","rol":"jugador"}));
}

#[tokio::test(start_paused = true)]
async fn turno_nuevo_de_otro_jugador_o_de_la_misma_cuenta_corta_al_anterior() {
    for siguiente in [("u2", "t2"), ("u1", "t2")] {
        let b = banco_con_turno().await;
        let (mut ws, _) = b.ws_con(&token_jugador("u1", "t1")).await;
        enviar(&mut ws, estado_json(CRUZ)).await;
        sleep(Duration::from_millis(5)).await;
        b.empujar(2, Some(siguiente)).await;
        assert_eq!(b.ultimo_mando(), EstadoMando::suelto(), "{siguiente:?}");
        assert_eq!(recibir(&mut ws).await, Some(json!({"t":"error","codigo":"TURNO"})), "{siguiente:?}");
        assert!(cerrado(&mut ws).await);
    }
}

#[tokio::test(start_paused = true)]
async fn turno_con_el_mismo_jugador_y_otra_version_no_lo_corta() {
    let b = banco_con_turno().await;
    let (mut ws, _) = b.ws_con(&token_jugador("u1", "t1")).await;
    enviar(&mut ws, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    let antes = b.mandos().len();
    b.empujar(2, Some(("u1", "t1"))).await;
    assert_eq!(b.mandos().len(), antes, "ni un suelto");
    assert!(tokio::time::timeout(Duration::from_millis(100), recibir(&mut ws)).await.is_err(), "sin mensajes");
    enviar(&mut ws, estado_json(L1)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(L1), "sigue mandando");
}

#[tokio::test(start_paused = true)]
async fn turno_no_toca_al_operador() {
    let b = banco_con_turno().await;
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws, estado_json(PS)).await;
    sleep(Duration::from_millis(5)).await;
    b.empujar(2, None).await;
    assert_eq!(b.ultimo_mando(), con(PS));
    assert!(tokio::time::timeout(Duration::from_millis(100), recibir(&mut ws)).await.is_err());
}

/// Cuerpo de `/control/turno` con la fila en pausa (`"mandos":false`, sin vistas).
fn cuerpo_pausa(version: u64, jugador: Option<(&str, &str)>) -> String {
    let j = jugador.map(|(sub, t)| json!({"sub":sub,"turnId":t})).unwrap_or(Value::Null);
    json!({"version":version,"jugador":j,"vistas":[],"mandos":false}).to_string()
}

#[tokio::test(start_paused = true)]
async fn turno_sin_mandos_corta_al_jugador_con_pausa_sin_cortarle_el_video() {
    let b = banco_con_turno().await;
    let whep = uuid(1);
    assert_eq!(b.ver(&token_jugador("u1", "t1"), Some(&whep)).await, StatusCode::OK);
    let (mut ws, _) = b.ws_con(&token_jugador("u1", "t1")).await;
    enviar(&mut ws, json!({"t":"estado","botones":CRUZ,"l2":0,"r2":255,"lx":100,"ly":-100,"rx":0,"ry":0})).await;
    sleep(Duration::from_millis(5)).await;
    assert_ne!(b.ultimo_mando(), EstadoMando::suelto());
    let (st, r) = b.turno(&cuerpo_pausa(2, Some(("u1", "t1")))).await;
    assert_eq!((st, r.as_str()), (StatusCode::OK, r#"{"ok":true}"#));
    // Cuando llega el 200, la PS5 ya recibió el todo suelto (botones, gatillos y palancas al centro).
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto());
    assert_eq!(recibir(&mut ws).await, Some(json!({"t":"error","codigo":"PAUSA"})));
    assert!(cerrado(&mut ws).await);
    let m = b.mandos();
    let i = m.iter().rposition(|e| *e != EstadoMando::suelto()).unwrap();
    assert_eq!(&m[i + 1..], &[EstadoMando::suelto()], "suelto exactamente una vez: {m:?}");
    assert_eq!(b.salud_jugador().await["mando"], Value::Null, "la ranura quedó libre");
    // El video sigue: su WHEP no se expulsa y uno nuevo se autoriza (sin `id`: con otro, el de la misma
    // cuenta reemplazaría al abierto).
    assert!(b.expulsiones().await.is_empty(), "{:?}", b.expulsiones().await);
    assert_eq!(b.ver(&token_jugador("u1", "t1"), None).await, StatusCode::OK);
    // Durante la pausa, una conexión nueva recibe PAUSA sin tocar la PS5 (la web reintenta cada 2 s).
    let antes = b.mandos().len();
    let (mut ws, r) = b.ws_con(&token_jugador("u1", "t1")).await;
    assert_eq!(r, json!({"t":"error","codigo":"PAUSA"}));
    assert!(cerrado(&mut ws).await);
    assert_eq!(b.mandos().len(), antes);
    // Otro jugador sigue recibiendo TURNO.
    let (_, r) = b.ws_con(&token_jugador("u2", "t2")).await;
    assert_eq!(r, json!({"t":"error","codigo":"TURNO"}));
    // Vuelven los mandos (otra versión, sin la clave): reconecta y manda.
    b.empujar(3, Some(("u1", "t1"))).await;
    let (mut ws, r) = b.ws_con(&token_jugador("u1", "t1")).await;
    assert_eq!(r, json!({"t":"ok","rol":"jugador"}));
    enviar(&mut ws, estado_json(L1)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(L1));
    // `"mandos":true` explícito también se acepta y no lo corta.
    let cuerpo = json!({"version":4,"jugador":{"sub":"u1","turnId":"t1"},"vistas":[],"mandos":true}).to_string();
    let (st, r) = b.turno(&cuerpo).await;
    assert_eq!((st, r.as_str()), (StatusCode::OK, r#"{"ok":true}"#));
    assert!(tokio::time::timeout(Duration::from_millis(100), recibir(&mut ws)).await.is_err(), "sin mensajes");
    assert!(b.expulsiones().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn turno_sin_mandos_no_toca_al_operador_y_le_deja_entrar() {
    let b = banco_con_turno().await;
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws, estado_json(PS)).await;
    sleep(Duration::from_millis(5)).await;
    let (st, _) = b.turno(&cuerpo_pausa(2, Some(("u1", "t1")))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(b.ultimo_mando(), con(PS));
    assert!(tokio::time::timeout(Duration::from_millis(100), recibir(&mut ws)).await.is_err());
    drop(ws);
    sleep(Duration::from_millis(5)).await;
    let (_ws, r) = b.ws_con(&token_operador("op2")).await;
    assert_eq!(r, json!({"t":"ok","rol":"operador"}), "en pausa el operador también entra");
}

#[tokio::test(start_paused = true)]
async fn ctl_una_vista_recibe_rol_y_no_suelta_al_jugador() {
    let b = banco();
    b.turno(&json!({"version":1,"jugador":{"sub":"u1","turnId":"t1"},"vistas":["u2"]}).to_string()).await;
    let (mut ws, _) = b.ws_con(&token_jugador("u1", "t1")).await;
    enviar(&mut ws, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    let antes = b.mandos();
    for t in [token_vista("u2"), token_vista("u9")] {
        let (mut wv, r) = b.ws_con(&t).await;
        assert_eq!(r, json!({"t":"error","codigo":"ROL"}));
        assert!(cerrado(&mut wv).await);
    }
    assert_eq!(b.mandos(), antes, "la sesión no recibió ningún suelto");
    assert!(tokio::time::timeout(Duration::from_millis(100), recibir(&mut ws)).await.is_err(), "el jugador sigue");
    enviar(&mut ws, estado_json(L1)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(L1));
}

#[tokio::test(start_paused = true)]
async fn ctl_el_operador_reemplaza_al_jugador_y_le_dice_por_quien() {
    let b = banco_con_turno().await;
    let (mut wj, _) = b.ws_con(&token_jugador("u1", "t1")).await;
    enviar(&mut wj, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    let (_wo, r) = b.ws_con(&token("operador")).await;
    assert_eq!(r, json!({"t":"ok","rol":"operador"}));
    assert_eq!(recibir(&mut wj).await, Some(json!({"t":"reemplazado","por":"operador"})));
    assert!(cerrado(&mut wj).await);
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto());
}

#[tokio::test(start_paused = true)]
async fn ctl_dos_jugadores_con_la_misma_credencial_el_segundo_reemplaza() {
    let b = banco_con_turno().await;
    let (mut w1, _) = b.ws_con(&token_jugador("u1", "t1")).await;
    let (mut w2, r) = b.ws_con(&token_jugador("u1", "t1")).await;
    assert_eq!(r, json!({"t":"ok","rol":"jugador"}));
    assert_eq!(recibir(&mut w1).await, Some(json!({"t":"reemplazado","por":"jugador"})));
    assert!(cerrado(&mut w1).await);
    enviar(&mut w2, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.ultimo_mando(), con(CRUZ));
}

// ---------------------------------------------------------------- salud del jugador

fn r2_json() -> Value {
    json!({"t":"estado","botones":0,"l2":0,"r2":255,"lx":0,"ly":0,"rx":0,"ry":0})
}

#[tokio::test(start_paused = true)]
async fn salud_jugador_sin_empuje_es_todo_null() {
    let b = banco();
    assert_eq!(b.salud_jugador().await, json!({"version":null,"turnId":null,"mando":null,"inactivoMs":null}));
}

#[tokio::test(start_paused = true)]
async fn salud_jugador_empujado_y_sin_nadie_cuenta_la_inactividad() {
    let b = banco_con_turno().await;
    sleep(Duration::from_secs(91)).await;
    let j = b.salud_jugador().await;
    assert_eq!((j["version"].clone(), j["turnId"].clone(), j["mando"].clone()), (json!(1), json!("t1"), Value::Null));
    assert!(j["inactivoMs"].as_u64().unwrap() >= 90_000, "{j}");
}

#[tokio::test(start_paused = true)]
async fn salud_jugador_con_r2_mantenido_es_cero_y_al_soltar_vuelve_a_contar() {
    let b = banco_con_turno().await;
    let (mut ws, _) = b.ws_con(&token_jugador("u1", "t1")).await;
    sleep(Duration::from_secs(10)).await;
    enviar(&mut ws, r2_json()).await;
    for _ in 0..10 {
        sleep(Duration::from_millis(300)).await;
        enviar(&mut ws, json!({"t":"latido"})).await;
    }
    sleep(Duration::from_millis(5)).await;
    let j = b.salud_jugador().await;
    assert_eq!((j["inactivoMs"].clone(), j["mando"].clone()), (json!(0), json!("jugador")), "{j}");
    // 500 ms de silencio sueltan todo: desde ahí cuenta.
    sleep(Duration::from_millis(1_500)).await;
    let j = b.salud_jugador().await;
    let ms = j["inactivoMs"].as_u64().unwrap();
    assert!((1_000..=1_300).contains(&ms), "{j}");
    // La deriva de un stick no es actividad.
    enviar(&mut ws, json!({"t":"estado","botones":0,"l2":0,"r2":0,"lx":6000,"ly":0,"rx":0,"ry":0})).await;
    sleep(Duration::from_millis(5)).await;
    assert!(b.salud_jugador().await["inactivoMs"].as_u64().unwrap() >= 1_000);
    // Vuelve a apretar y cierra la conexión con R2 apretado: cuenta desde el cierre.
    enviar(&mut ws, r2_json()).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.salud_jugador().await["inactivoMs"], json!(0));
    ws.close(None).await.unwrap();
    sleep(Duration::from_millis(5)).await;
    sleep(Duration::from_secs(2)).await;
    let j = b.salud_jugador().await;
    let ms = j["inactivoMs"].as_u64().unwrap();
    assert!((2_000..=2_100).contains(&ms), "{j}");
    assert_eq!(j["mando"], Value::Null, "nadie en la ranura");
}

#[tokio::test(start_paused = true)]
async fn salud_jugador_con_el_operador_dice_operador_y_su_mando_no_es_actividad() {
    let b = banco_con_turno().await;
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    sleep(Duration::from_secs(5)).await;
    enviar(&mut ws, r2_json()).await;
    sleep(Duration::from_millis(5)).await;
    let j = b.salud_jugador().await;
    assert_eq!(j["mando"], json!("operador"));
    assert!(j["inactivoMs"].as_u64().unwrap() >= 5_000, "{j}");
}

#[tokio::test(start_paused = true)]
async fn salud_jugador_reemplazado_por_el_operador_deja_de_contar_como_activo() {
    let b = banco_con_turno().await;
    let (mut wj, _) = b.ws_con(&token_jugador("u1", "t1")).await;
    enviar(&mut wj, r2_json()).await;
    sleep(Duration::from_millis(5)).await;
    assert_eq!(b.salud_jugador().await["inactivoMs"], json!(0));
    let (mut wo, _) = b.ws_con(&token("operador")).await;
    for _ in 0..10 {
        sleep(Duration::from_millis(300)).await;
        enviar(&mut wo, json!({"t":"latido"})).await;
    }
    let j = b.salud_jugador().await;
    assert!(j["inactivoMs"].as_u64().unwrap() >= 2_900, "{j}");
}

#[tokio::test(start_paused = true)]
async fn tras_un_reinicio_el_jugador_recibe_turno_hasta_el_empuje_y_despues_ok() {
    // Review Focus 5: miturno-rp arranca sin turno vigente (nada en disco).
    let b = banco();
    let t = token_jugador("u1", "t1");
    let (_, r) = b.ws_con(&t).await;
    assert_eq!(r, json!({"t":"error","codigo":"TURNO"}));
    // La api tarda en reenviar: la inactividad cuenta desde el empuje, no desde el arranque.
    sleep(Duration::from_secs(30)).await;
    b.empujar(7, Some(("u1", "t1"))).await;
    let j = b.salud_jugador().await;
    assert_eq!((j["version"].clone(), j["turnId"].clone()), (json!(7), json!("t1")));
    assert!(j["inactivoMs"].as_u64().is_some_and(|ms| ms <= 100), "{j}");
    let (_ws, r) = b.ws_con(&t).await;
    assert_eq!(r, json!({"t":"ok","rol":"jugador"}));
}

#[tokio::test(start_paused = true)]
async fn ctl_los_rechazos_turno_rol_y_ocupado_pasan_por_el_limitador() {
    // /ctl es público: un rechazo por turno, rol u ocupado no puede llenar el journal (uno por minuto).
    use crate::registro::{ms_desde_arranque, CADA_RECHAZO_MS};
    let b = banco();
    for (t, codigo) in [(token_jugador("u1", "t1"), "TURNO"), (token_vista("u2"), "ROL")] {
        let (_, r) = b.ws_con(&t).await;
        assert_eq!(r, json!({"t":"error","codigo":codigo}));
        assert_eq!(
            ctl::RECHAZO_MANDOS.permitir(ms_desde_arranque(), CADA_RECHAZO_MS),
            None,
            "{codigo}: el limitador ya registró un aviso en el último minuto"
        );
    }
    b.empujar(1, Some(("u1", "t1"))).await;
    let (_wo, r) = b.ws_con(&token("operador")).await;
    assert_eq!(r, json!({"t":"ok","rol":"operador"}));
    let (_, r) = b.ws_con(&token_jugador("u1", "t1")).await;
    assert_eq!(r, json!({"t":"error","codigo":"OCUPADO"}));
    assert_eq!(ctl::RECHAZO_OCUPADO.permitir(ms_desde_arranque(), CADA_RECHAZO_MS), None, "OCUPADO");
}

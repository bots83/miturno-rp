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

fn claims(rol: &str) -> Value {
    json!({"iss":"miturno","aud":"miturno-video","sub":"u1","rol":rol,"iat":AHORA - 10,"exp":AHORA + 3600,"jti":"j1"})
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

fn banco() -> Banco {
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

    async fn auth_mediamtx(&self, cuerpo: Value) -> StatusCode {
        self.pedir("POST", "/interno/auth", local(), None, &cuerpo.to_string()).await.0
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
    let b = banco();
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
    let b = banco();
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
    assert_eq!(recibir(&mut ws1).await, Some(json!({"t":"reemplazado"})));
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
    let b = banco();
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
    let b = banco();
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
    let b = banco();
    let e = &b.estado;
    let _r1 = e.tomar(1, rp_core::Rol::Operador).unwrap();
    let _r2 = e.tomar(2, rp_core::Rol::Operador).unwrap();
    let antes = b.mandos().len();
    e.mandar(1, &con(CRUZ));
    e.soltar(1);
    assert_eq!(b.mandos().len(), antes, "la conexión reemplazada ni manda ni suelta");
    e.mandar(2, &con(PS));
    assert_eq!(b.ultimo_mando(), con(PS));
    assert!(e.tomar(3, rp_core::Rol::Jugador).is_none(), "un rol menor no toma");
    e.soltar(2);
    assert_eq!(b.ultimo_mando(), EstadoMando::suelto());
    assert!(e.tomar(3, rp_core::Rol::Jugador).is_some(), "libre, cualquier rol toma");
}

#[tokio::test(start_paused = true)]
async fn ctl_un_panico_en_la_conexion_libera_la_ranura_y_suelta_una_vez() {
    let b = banco();
    b.estado.panico_en_estado.store(true, std::sync::atomic::Ordering::SeqCst);
    let (mut ws, _) = b.ws_con(&token("operador")).await;
    enviar(&mut ws, estado_json(CRUZ)).await;
    sleep(Duration::from_millis(5)).await;
    let m = b.mandos();
    let i = m.iter().position(|e| *e == con(CRUZ)).expect("llegó el estado antes del pánico");
    assert_eq!(&m[i + 1..], &[EstadoMando::suelto()], "suelto exactamente una vez: {m:?}");
    assert!(b.estado.ranura.lock().unwrap().is_none(), "la ranura quedó libre");
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
        let (_dueno, _aviso) = e.tomar(7, rp_core::Rol::Operador).unwrap();
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
    assert!(b.estado.ranura.lock().unwrap().is_none());
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
    let b = banco();
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
        (auth("api", "", "127.0.0.1", "", ""), StatusCode::UNAUTHORIZED),
        (auth("metrics", "", "127.0.0.1", "", ""), StatusCode::UNAUTHORIZED),
        (auth("playback", "", "127.0.0.1", "ps5", &op), StatusCode::UNAUTHORIZED),
        (auth("pprof", "", "127.0.0.1", "", ""), StatusCode::UNAUTHORIZED),
    ];
    for (cuerpo, esperado) in casos {
        assert_eq!(b.auth_mediamtx(cuerpo.clone()).await, esperado, "{cuerpo}");
    }
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
            "cpu": 0.0
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

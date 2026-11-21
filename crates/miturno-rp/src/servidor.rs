//! Servidor HTTP/WebSocket local de miturno-rp (escucha en `http` de `rp.toml`, 127.0.0.1:7070).
//!
//! - `GET /ctl` (WebSocket, detrás de Caddy): los mandos de quien juega, con la credencial de turno.
//! - `POST /interno/auth` (autenticación externa de MediaMTX) y `POST /interno/keyframe`: solo desde
//!   la misma máquina y nunca a través del proxy.
//! - `GET /control/salud` y `POST /control/{reconectar,kick_on,kick_off,reposo,encender}`: solo con el
//!   token de control de la api (nunca con la credencial del navegador).
//!
//! Nunca se registran credenciales, tokens ni el nombre de la conexión de casa.

mod ctl;
#[cfg(test)]
mod tests;

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rp_core::{validar_con, ClaveVerificacion, Rol};
use serde::{Deserialize, Serialize};
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::ciclo::Orden;
use crate::metricas::{FotoSesion, Metricas};
use crate::sesion::{ConLimite, SesionPs5};
use crate::sistema::{cpu_por_ciento, Sistema};

/// Cada cuánto se muestrea la CPU de la máquina para la salud.
pub const INTERVALO_CPU: Duration = Duration::from_secs(2);
/// Unidades de systemd que mira la salud. `kick_on`/`kick_off` arrancan y paran la primera (la regla
/// de polkit de la instalación permite exactamente eso al usuario `rp`).
pub const UNIDAD_KICK: &str = "miturno-kick-out.service";
pub const UNIDAD_MEDIAMTX: &str = "mediamtx.service";
pub const UNIDAD_TURN: &str = "coturn.service";
/// La única ruta de MediaMTX que se puede ver por WebRTC (WHEP).
pub const RUTA_PS5: &str = "ps5";
/// Largo mínimo del token de control.
const MINIMO_TOKEN: usize = 32;

/// Token de servidor a servidor de `/control/*`. Se lee una vez al arrancar y nunca se muestra.
pub struct TokenControl(Vec<u8>);

impl std::fmt::Debug for TokenControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenControl(<oculto>)")
    }
}

impl TokenControl {
    /// El texto se recorta (el archivo suele terminar con un salto de línea).
    pub fn new(texto: &str) -> Result<Self, String> {
        let t = texto.trim();
        if t.len() < MINIMO_TOKEN {
            return Err(format!("el token de control está vacío o es demasiado corto (mínimo {MINIMO_TOKEN} caracteres)"));
        }
        Ok(Self(t.as_bytes().to_vec()))
    }

    /// Comparación en tiempo constante (para un mismo largo).
    pub fn coincide(&self, otro: &[u8]) -> bool {
        bool::from(self.0.as_slice().ct_eq(otro))
    }
}

/// Lee y valida la clave pública de las credenciales. Falla al arrancar, con un error claro, si no es
/// una clave Ed25519 (así una clave rota del servidor no parece un error de firma del cliente).
pub fn cargar_clave_publica(ruta: &Path) -> Result<ClaveVerificacion, String> {
    let pem = crate::config::leer(ruta).map_err(|e| e.to_string())?;
    ClaveVerificacion::desde_pem(&pem).map_err(|e| format!("{}: {e}", ruta.display()))
}

/// Lee el token de control. El error dice qué archivo falla, nunca el contenido.
pub fn cargar_token_control(ruta: &Path) -> Result<TokenControl, String> {
    let texto = crate::config::leer(ruta).map_err(|e| e.to_string())?;
    TokenControl::new(&texto).map_err(|e| format!("{}: {e}", ruta.display()))
}

/// Hora de pared en segundos Unix: solo para comparar con el `exp` de las credenciales.
pub fn reloj_sistema_s() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Todo lo que necesita el servidor.
pub struct Piezas<S: SesionPs5> {
    /// La sesión detrás del limitador único de keyframes (el mismo `Arc` que usa el ciclo).
    pub sesion: Arc<ConLimite<S>>,
    pub clave: ClaveVerificacion,
    pub token_control: TokenControl,
    pub ordenes: mpsc::Sender<Orden>,
    pub metricas: Arc<Metricas>,
    pub sistema: Arc<dyn Sistema>,
    /// Hora de pared en segundos (inyectable para los tests).
    pub reloj_s: fn() -> u64,
}

pub struct EstadoServidor<S: SesionPs5> {
    sesion: Arc<ConLimite<S>>,
    clave: ClaveVerificacion,
    token_control: TokenControl,
    ordenes: mpsc::Sender<Orden>,
    metricas: Arc<Metricas>,
    sistema: Arc<dyn Sistema>,
    reloj_s: fn() -> u64,
    /// Origen del reloj monótono (en milisegundos) del filtro y del guardián.
    t0: Instant,
    /// El único controlador que puede mandar a la PS5.
    ranura: Mutex<Option<ctl::Ocupante>>,
    proximo_id: AtomicU64,
    cpu: Mutex<f64>,
    /// Toques descartados por el filtro, sumados de todas las conexiones terminadas.
    descartados: AtomicU64,
    /// Solo tests: la conexión entra en pánico después de aplicar un `estado`.
    #[cfg(test)]
    panico_en_estado: std::sync::atomic::AtomicBool,
}

impl<S: SesionPs5> EstadoServidor<S> {
    pub fn new(p: Piezas<S>) -> Arc<Self> {
        Arc::new(Self {
            sesion: p.sesion,
            clave: p.clave,
            token_control: p.token_control,
            ordenes: p.ordenes,
            metricas: p.metricas,
            sistema: p.sistema,
            reloj_s: p.reloj_s,
            t0: Instant::now(),
            ranura: Mutex::new(None),
            proximo_id: AtomicU64::new(1),
            cpu: Mutex::new(0.0),
            descartados: AtomicU64::new(0),
            #[cfg(test)]
            panico_en_estado: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Toques descartados por el filtro de rol en las conexiones de `/ctl` ya terminadas.
    pub fn descartados(&self) -> u64 {
        self.descartados.load(std::sync::atomic::Ordering::Relaxed)
    }
}

type Compartido<S> = Arc<EstadoServidor<S>>;

pub fn rutas<S: SesionPs5>(estado: Compartido<S>) -> Router {
    let interno = Router::new()
        .route("/interno/auth", post(auth_mediamtx::<S>))
        .route("/interno/keyframe", post(keyframe::<S>))
        .route_layer(middleware::from_fn(solo_local));
    let control = Router::new()
        .route("/control/salud", get(salud::<S>))
        .route("/control/reconectar", post(|State(e): State<Compartido<S>>| async move { ordenar(&e, Orden::Reconectar) }))
        .route("/control/reposo", post(|State(e): State<Compartido<S>>| async move { ordenar(&e, Orden::Reposo) }))
        .route("/control/encender", post(|State(e): State<Compartido<S>>| async move { ordenar(&e, Orden::Encender) }))
        .route("/control/kick_on", post(|State(e): State<Compartido<S>>| async move { kick(&e, "start").await }))
        .route("/control/kick_off", post(|State(e): State<Compartido<S>>| async move { kick(&e, "stop").await }))
        .route_layer(middleware::from_fn_with_state(estado.clone(), con_token::<S>));
    Router::new().route("/ctl", get(ctl::ctl::<S>)).merge(interno).merge(control).with_state(estado)
}

/// Sirve las rutas con la IP de origen de cada conexión (`ConnectInfo`).
pub async fn servir<S, L>(listener: L, estado: Compartido<S>)
where
    S: SesionPs5,
    L: axum::serve::Listener<Addr = SocketAddr>,
{
    use axum::serve::ListenerExt;
    let app = rutas(estado).into_make_service_with_connect_info::<SocketAddr>();
    if let Err(e) = axum::serve(listener.tap_io(|_| {}), app).await {
        crate::falla!("servidor: {e}");
    }
}

/// Muestrea la CPU de toda la máquina cada `INTERVALO_CPU` (corre para siempre).
pub async fn muestrear_cpu<S: SesionPs5>(estado: Compartido<S>) {
    let mut previa = estado.sistema.tiempos_cpu();
    loop {
        tokio::time::sleep(INTERVALO_CPU).await;
        let ahora = estado.sistema.tiempos_cpu();
        if let (Some(a), Some(b)) = (previa, ahora) {
            *estado.cpu.lock().unwrap_or_else(|e| e.into_inner()) = cpu_por_ciento(a, b);
        }
        if ahora.is_some() {
            previa = ahora;
        }
    }
}

fn codigo(estado: StatusCode, codigo: &str) -> Response {
    (estado, Json(json!({ "codigo": codigo }))).into_response()
}

fn ok() -> Response {
    Json(json!({ "ok": true })).into_response()
}

// ---------------------------------------------------------------- /interno

/// Solo la propia máquina, y nunca a través de Caddy (que agrega `X-Forwarded-For`): Caddy también
/// se conecta desde 127.0.0.1, así que la IP sola no alcanza.
async fn solo_local(ConnectInfo(par): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    let h = req.headers();
    let por_proxy = h.contains_key("x-forwarded-for") || h.contains_key(header::FORWARDED) || h.contains_key("x-real-ip");
    if !par.ip().to_canonical().is_loopback() || por_proxy {
        crate::aviso!("servidor: pedido a {} rechazado (no es local)", req.uri().path());
        return codigo(StatusCode::FORBIDDEN, "LOCAL");
    }
    next.run(req).await
}

/// Lo que manda MediaMTX (v1.21) a la autenticación externa. Los campos nuevos se ignoran.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PedidoAuth {
    token: Option<String>,
    ip: Option<String>,
    action: Option<String>,
    path: Option<String>,
    protocol: Option<String>,
}

/// - `publish` y `read` por RTSP o UDP desde la máquina: ffmpeg de Kick, la entrada UDP y (F2) el vigilante;
/// - `read` por WebRTC (WHEP) de `ps5` con una credencial válida de `operador` (en F1 solo pasa el operador);
/// - todo lo demás (API, métricas, HLS, RTMP, SRT, otras rutas): no.
fn permitir_mediamtx(p: &PedidoAuth, clave: &ClaveVerificacion, ahora_s: u64) -> bool {
    let s = |o: &Option<String>| o.as_deref().unwrap_or("").to_string();
    let local = s(&p.ip).parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical().is_loopback());
    match (s(&p.action).as_str(), s(&p.protocol).as_str()) {
        ("publish" | "read", "rtsp" | "udp") => local,
        ("read", "webrtc") => {
            s(&p.path) == RUTA_PS5
                && validar_con(&s(&p.token), clave, ahora_s).is_ok_and(|c| c.rol == Rol::Operador)
        }
        _ => false,
    }
}

async fn auth_mediamtx<S: SesionPs5>(State(e): State<Compartido<S>>, cuerpo: Bytes) -> StatusCode {
    let Ok(p) = serde_json::from_slice::<PedidoAuth>(&cuerpo) else {
        crate::aviso!("interno/auth: cuerpo inválido; rechazado");
        return StatusCode::UNAUTHORIZED;
    };
    let permitido = permitir_mediamtx(&p, &e.clave, (e.reloj_s)());
    crate::depurar!(
        "interno/auth: {} por {} en {:?}: {}",
        p.action.as_deref().unwrap_or("?"),
        p.protocol.as_deref().unwrap_or("?"),
        p.path.as_deref().unwrap_or(""),
        if permitido { "permitido" } else { "rechazado" }
    );
    if permitido {
        StatusCode::OK
    } else {
        StatusCode::UNAUTHORIZED
    }
}

/// Pedido de keyframe del hook de MediaMTX o de kick-out. Pasa por el limitador único: `enviado: false`
/// quiere decir que cayó dentro de la ventana de 3 s y sale diferido al final de la ventana.
async fn keyframe<S: SesionPs5>(State(e): State<Compartido<S>>) -> Json<serde_json::Value> {
    let enviado = e.sesion.pedir_keyframe();
    crate::depurar!("interno/keyframe: {}", if enviado { "pedido" } else { "diferido" });
    Json(json!({ "enviado": enviado }))
}

// ---------------------------------------------------------------- /control

async fn con_token<S: SesionPs5>(State(e): State<Compartido<S>>, req: Request, next: Next) -> Response {
    let valido = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "))
        .is_some_and(|t| e.token_control.coincide(t));
    if !valido {
        crate::aviso!("control: pedido a {} sin token válido", req.uri().path());
        return codigo(StatusCode::UNAUTHORIZED, "TOKEN");
    }
    next.run(req).await
}

fn ordenar<S: SesionPs5>(e: &EstadoServidor<S>, orden: Orden) -> Response {
    match e.ordenes.try_send(orden) {
        Ok(()) => {
            crate::info!("control: orden {orden:?}");
            ok()
        }
        Err(_) => {
            crate::falla!("control: el ciclo no recibe órdenes ({orden:?})");
            codigo(StatusCode::SERVICE_UNAVAILABLE, "CICLO")
        }
    }
}

async fn kick<S: SesionPs5>(e: &EstadoServidor<S>, verbo: &str) -> Response {
    match e.sistema.systemctl(verbo, UNIDAD_KICK).await {
        Ok(()) => {
            crate::info!("control: salida a Kick: {verbo}");
            ok()
        }
        Err(m) => {
            crate::falla!("control: {m}");
            codigo(StatusCode::BAD_GATEWAY, "SYSTEMCTL")
        }
    }
}

/// `VideoSalud.kick` (los nombres y valores los fija el tipo de la api).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Kick {
    pub encendido: bool,
    pub estado: &'static str,
}

/// `systemctl is-active` de kick-out → salud. `activating` es el reinicio automático tras una caída
/// (la unidad es de tipo simple): encendida, pero con error.
pub fn estado_kick(activo: &str) -> Kick {
    let (encendido, estado) = match activo {
        "active" | "reloading" => (true, "transmitiendo"),
        "activating" => (true, "error"),
        "inactive" | "deactivating" => (false, "apagado"),
        _ => (false, "error"),
    };
    Kick { encendido, estado }
}

/// La forma exacta de `VideoSalud` de la api.
#[derive(Debug, Serialize)]
struct VideoSalud {
    sesion: FotoSesion,
    kick: Kick,
    mediamtx: bool,
    turn: bool,
    cpu: f64,
}

async fn salud<S: SesionPs5>(State(e): State<Compartido<S>>) -> Json<VideoSalud> {
    let unidades = [UNIDAD_KICK, UNIDAD_MEDIAMTX, UNIDAD_TURN];
    let estados = e.sistema.estados(&unidades).await;
    let activo = |i: usize| estados.get(i).map(String::as_str).unwrap_or("desconocido");
    Json(VideoSalud {
        sesion: e.metricas.foto(Instant::now()),
        kick: estado_kick(activo(0)),
        mediamtx: activo(1) == "active",
        turn: activo(2) == "active",
        cpu: *e.cpu.lock().unwrap_or_else(|e| e.into_inner()),
    })
}

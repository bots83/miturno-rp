//! Sesión real con la PS5 sobre libchiaki (chiaki-ng v1.10.1), por conexión directa al nombre DDNS de casa.
//!
//! Adaptado de chiaki-stream (nanpuyue), `src/run.rs` (`cmd_stream` y `cmd_wakeup`), AGPL-3.0: el armado de
//! `ConnectInfo`, los callbacks de video y audio, el reintento del keyframe ante fallas de FEC y el cierre
//! con `stop()` + `join()`. Cambios: la sesión vive en un hilo propio que recibe comandos (así se la
//! maneja desde cualquier tarea), se reconecta sin reiniciar el proceso y los eventos salen por un canal.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libchiaki::{
    ffi, AudioHeader, ConnectInfo, ControllerState, Discovery, Event, Log, LogLevel, QuitReason, Session, VideoSample,
};
use rp_core::{mando, EstadoMando};
use tokio::sync::{mpsc, oneshot};

use super::{ErrorSesion, EventoSesion, MotivoCierre, SesionPs5};
use crate::config::Perfil;

/// ~4 s de video a 60 fps: absorbe frenadas cortas del ciclo sin perder cuadros.
const CAPACIDAD: usize = 256;

enum Cmd {
    Despertar(Perfil),
    Conectar(Perfil),
    Detener(oneshot::Sender<()>),
    /// Se soltó el último `SesionLibchiaki`: cerrar la sesión y terminar el hilo.
    Fin(std_mpsc::SyncSender<()>),
    Mando(EstadoMando),
    Keyframe,
    Reposo,
    // Desde los callbacks de libchiaki (con el número de sesión, para ignorar los de una sesión vieja).
    Terminada(u64),
    FalloFec { sesion: u64, pedido_enviado: bool },
    PidePin(u64),
}

pub struct SesionLibchiaki {
    cmd: std_mpsc::Sender<Cmd>,
    eventos: Mutex<Option<mpsc::Receiver<EventoSesion>>>,
}

impl SesionLibchiaki {
    pub fn new() -> Result<Self, String> {
        Self::con_contador(Arc::default())
    }

    /// `cierres` cuenta las sesiones cerradas con `stop()` (lo leen los tests).
    fn con_contador(cierres: Arc<AtomicUsize>) -> Result<Self, String> {
        static INICIO: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
        INICIO
            .get_or_init(|| libchiaki::lib_init().map_err(|e| format!("no pude iniciar libchiaki: {e}")))
            .clone()?;
        let (cmd, rx_cmd) = std_mpsc::channel();
        let (tx_ev, rx_ev) = mpsc::channel(CAPACIDAD);
        let propio = cmd.clone();
        let c = cierres.clone();
        std::thread::Builder::new()
            .name("sesion-ps5".into())
            .spawn(move || hilo(rx_cmd, propio, tx_ev, c))
            .map_err(|e| format!("no pude crear el hilo de la sesión: {e}"))?;
        Ok(Self { cmd, eventos: Mutex::new(Some(rx_ev)) })
    }

    fn mandar(&self, c: Cmd) -> Result<(), ErrorSesion> {
        self.cmd.send(c).map_err(|_| ErrorSesion::HiloTerminado)
    }
}

/// Al soltar el último handle (fin normal, o un pánico que desenrolla la tarea dueña) la sesión se
/// cierra con `stop()` antes de seguir: si no, la PS5 rechaza la próxima con "Remote is already in use".
/// El hilo guarda su propio `Sender` (para los callbacks), así que sin este aviso nunca terminaría.
impl Drop for SesionLibchiaki {
    fn drop(&mut self) {
        let (listo, esperar) = std_mpsc::sync_channel(1);
        if self.cmd.send(Cmd::Fin(listo)).is_ok() && esperar.recv_timeout(Duration::from_secs(4)).is_err() {
            crate::aviso!("sesión: el cierre al salir no terminó en 4 s");
        }
    }
}

impl SesionPs5 for SesionLibchiaki {
    fn despertar(&self, perfil: &Perfil) -> Result<(), ErrorSesion> {
        self.mandar(Cmd::Despertar(perfil.clone()))
    }

    fn conectar(&self, perfil: &Perfil) -> Result<(), ErrorSesion> {
        self.mandar(Cmd::Conectar(perfil.clone()))
    }

    fn detener(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        let enviado = self.mandar(Cmd::Detener(tx)).is_ok();
        async move {
            if enviado {
                let _ = rx.await;
            }
        }
    }

    fn poner_mando(&self, estado: &EstadoMando) {
        let _ = self.mandar(Cmd::Mando(*estado));
    }

    fn pedir_keyframe(&self) -> bool {
        self.mandar(Cmd::Keyframe).is_ok()
    }

    fn reposo(&self) {
        let _ = self.mandar(Cmd::Reposo);
    }

    fn eventos(&self) -> Option<mpsc::Receiver<EventoSesion>> {
        self.eventos.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Correspondencia explícita de nuestros bits (`rp_core::mando`) con los de libchiaki. Nunca se pasan
/// nuestros bits tal cual: el orden de libchiaki es otro.
const BOTONES: [(u32, u32); 16] = [
    (mando::CRUZ, libchiaki::BUTTON_CROSS),
    (mando::CIRCULO, libchiaki::BUTTON_MOON),
    (mando::CUADRADO, libchiaki::BUTTON_BOX),
    (mando::TRIANGULO, libchiaki::BUTTON_PYRAMID),
    (mando::L1, libchiaki::BUTTON_L1),
    (mando::R1, libchiaki::BUTTON_R1),
    (mando::L3, libchiaki::BUTTON_L3),
    (mando::R3, libchiaki::BUTTON_R3),
    (mando::OPTIONS, libchiaki::BUTTON_OPTIONS),
    (mando::CREATE, libchiaki::BUTTON_SHARE),
    (mando::PS, libchiaki::BUTTON_PS),
    (mando::PANEL, libchiaki::BUTTON_TOUCHPAD),
    (mando::ARRIBA, libchiaki::BUTTON_DPAD_UP),
    (mando::ABAJO, libchiaki::BUTTON_DPAD_DOWN),
    (mando::IZQUIERDA, libchiaki::BUTTON_DPAD_LEFT),
    (mando::DERECHA, libchiaki::BUTTON_DPAD_RIGHT),
];

/// Nuestro estado del mando, en el formato de libchiaki. L2 y R2 van como valores analógicos (así los
/// manda chiaki-ng); los sticks pasan tal cual (mismo rango).
pub fn a_chiaki(e: &EstadoMando) -> ControllerState {
    let mut s = ControllerState::idle();
    s.0.buttons = BOTONES.iter().filter(|(nuestro, _)| e.botones & nuestro != 0).fold(0, |acc, (_, suyo)| acc | suyo);
    s.0.l2_state = e.l2;
    s.0.r2_state = e.r2;
    s.0.left_x = e.lx;
    s.0.left_y = e.ly;
    s.0.right_x = e.rx;
    s.0.right_y = e.ry;
    s
}

pub fn motivo(r: QuitReason) -> MotivoCierre {
    use ffi::ChiakiQuitReason::*;
    match r {
        CHIAKI_QUIT_REASON_SESSION_REQUEST_RP_IN_USE => MotivoCierre::EnUso,
        CHIAKI_QUIT_REASON_SESSION_REQUEST_UNKNOWN
        | CHIAKI_QUIT_REASON_SESSION_REQUEST_CONNECTION_REFUSED
        | CHIAKI_QUIT_REASON_CTRL_CONNECT_FAILED
        | CHIAKI_QUIT_REASON_CTRL_CONNECTION_REFUSED
        | CHIAKI_QUIT_REASON_STREAM_CONNECTION_UNKNOWN
        | CHIAKI_QUIT_REASON_STREAM_CONNECTION_REMOTE_DISCONNECTED
        | CHIAKI_QUIT_REASON_STREAM_CONNECTION_REMOTE_SHUTDOWN => MotivoCierre::Red,
        _ => MotivoCierre::Otro,
    }
}

/// El fin de sesión tal como lo da libchiaki, para el registro: el `ChiakiQuitReason` (nombre y número),
/// su texto, el texto que mandó la PS5 al desconectar (`reason_str`, viene solo con
/// `STREAM_CONNECTION_REMOTE_*`; tapado el nombre de casa) y el motivo que usa el ciclo.
fn describir_cierre(r: QuitReason, reason_str: &str) -> String {
    let texto = if reason_str.is_empty() { "sin texto de la PS5".to_owned() } else { format!("texto de la PS5: {:?}", ocultar(reason_str)) };
    format!("{:?} = {} ({:?}); {texto}; motivo {:?}", r, r as u32, libchiaki::quit_reason_string(r), motivo(r))
}

/// Textos privados (el nombre de casa) que se tapan en el registro de libchiaki.
static OCULTAR: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn ocultar(msg: &str) -> Cow<'_, str> {
    let lista = OCULTAR.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Cow::Borrowed(msg);
    for s in lista.iter().filter(|s| !s.is_empty() && msg.contains(s.as_str())) {
        out = Cow::Owned(out.replace(s.as_str(), "[casa]"));
    }
    out
}

fn registrar_privado(perfil: &Perfil) {
    let mut lista = OCULTAR.lock().unwrap_or_else(|e| e.into_inner());
    if !lista.contains(&perfil.host) {
        lista.push(perfil.host.clone());
    }
}

fn nuevo_log() -> Log {
    let e = LogLevel::CHIAKI_LOG_ERROR as u32;
    let w = LogLevel::CHIAKI_LOG_WARNING as u32;
    let i = LogLevel::CHIAKI_LOG_INFO as u32;
    let mascara = if crate::registro::depurando() { e | w | i } else { e | w };
    Log::new(mascara, |nivel, msg| {
        let msg = ocultar(msg);
        match nivel {
            LogLevel::CHIAKI_LOG_ERROR => crate::falla!("chiaki: {msg}"),
            LogLevel::CHIAKI_LOG_WARNING => crate::aviso!("chiaki: {msg}"),
            _ => crate::depurar!("chiaki: {msg}"),
        }
    })
}

/// Los avisos de control (conectada, cerrada) no se pueden perder: si el canal está lleno de video,
/// se reintenta un rato. Corre en un hilo de libchiaki, así que puede esperar.
fn avisar(ev: &mpsc::Sender<EventoSesion>, e: EventoSesion) {
    let mut e = e;
    for _ in 0..200 {
        match ev.try_send(e) {
            Ok(()) => return,
            Err(mpsc::error::TrySendError::Closed(_)) => return,
            Err(mpsc::error::TrySendError::Full(otra)) => {
                e = otra;
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    crate::falla!("sesión: no pude avisar {e:?} (canal lleno)");
}

struct Activa<'a> {
    sesion: Session<'a>,
    numero: u64,
    /// La cerramos nosotros: no hay que avisar `Cerrada`.
    a_pedido: Arc<AtomicBool>,
}

fn abrir<'a>(
    log: &'a Log,
    perfil: &Perfil,
    numero: u64,
    ev: &mpsc::Sender<EventoSesion>,
    cmd: &std_mpsc::Sender<Cmd>,
) -> Result<Activa<'a>, String> {
    let mut info = ConnectInfo::new(&perfil.host, &perfil.regist_key, true).map_err(|_| "PS5_HOST inválido".to_string())?;
    info.set_morning(&perfil.morning);
    info.set_psn_account_id(&perfil.account_id);
    info.set_video_preset(
        ffi::ChiakiVideoResolutionPreset::CHIAKI_VIDEO_RESOLUTION_PRESET_1080p,
        ffi::ChiakiVideoFPSPreset::CHIAKI_VIDEO_FPS_PRESET_60,
    );
    info.set_bitrate(perfil.bitrate_kbps);
    info.set_video_codec(ffi::ChiakiCodec::CHIAKI_CODEC_H264);
    // Si la corrección de errores no alcanza, libchiaki pide un keyframe solo y descarta los cuadros
    // rotos hasta que llegue (medido: la PS5 no manda keyframes sola; sin esto la imagen queda rota).
    info.set_enable_idr_on_fec_failure(true);

    let mut sesion = Session::new(info, log).map_err(|e| format!("no pude crear la sesión: {e}"))?;
    let a_pedido = Arc::new(AtomicBool::new(false));

    {
        let (ev, cmd, a_pedido) = (ev.clone(), cmd.clone(), a_pedido.clone());
        sesion.set_event_callback(move |e| match e {
            Event::Connected => avisar(&ev, EventoSesion::Conectada),
            Event::Quit { reason, reason_str } => {
                // Primero al hilo (para que haga join), después al ciclo: así un `conectar` nuevo llega
                // siempre después de liberar esta sesión.
                let _ = cmd.send(Cmd::Terminada(numero));
                if !a_pedido.load(Ordering::SeqCst) {
                    crate::info!("sesión: terminó: {}", describir_cierre(reason, &reason_str));
                    avisar(&ev, EventoSesion::Cerrada { motivo: motivo(reason) });
                }
            }
            Event::LoginPinRequest { .. } => {
                let _ = cmd.send(Cmd::PidePin(numero));
            }
            Event::VideoFecFailure { idr_request_sent, .. } => {
                let _ = cmd.send(Cmd::FalloFec { sesion: numero, pedido_enviado: idr_request_sent });
            }
            _ => {}
        });
    }
    {
        let ev = ev.clone();
        let mut descartados = 0u64;
        sesion.set_video_sample_callback(move |m: VideoSample| {
            let e = EventoSesion::Video { datos: m.data.to_vec(), perdidos: m.frames_lost.max(0) as u32 };
            match ev.try_send(e) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // Devolver false: chiaki-ng v1.10.1 no lo toma como cuadro de referencia y, con el
                    // cuadro siguiente, le manda a la PS5 un `send_corrupt_frame`; la PS5 se recupera sola
                    // (codifica contra un cuadro que sí llegó), sin esperar un IDR.
                    descartados += 1;
                    if descartados % 60 == 1 {
                        crate::aviso!("sesión: el ciclo no da abasto; van {descartados} cuadros descartados");
                    }
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }
    {
        let (ev_h, ev_a) = (ev.clone(), ev.clone());
        sesion.set_audio_sink(
            move |h: &mut AudioHeader| {
                let _ = ev_h.try_send(EventoSesion::FormatoAudio {
                    canales: h.0.channels,
                    frecuencia: h.0.rate,
                    muestras_por_paquete: h.0.frame_size,
                });
            },
            move |datos: &mut [u8]| {
                let _ = ev_a.try_send(EventoSesion::Audio { datos: datos.to_vec() });
            },
        );
    }
    sesion.start().map_err(|e| format!("no pude arrancar la sesión: {e}"))?;
    Ok(Activa { sesion, numero, a_pedido })
}

/// Cierre limpio: `stop()` y espera al hilo de libchiaki. Sin esto, la PS5 rechaza la sesión siguiente.
fn cerrar(activa: &mut Option<Activa<'_>>, cierres: &AtomicUsize) {
    if let Some(mut a) = activa.take() {
        a.a_pedido.store(true, Ordering::SeqCst);
        if let Err(e) = a.sesion.stop() {
            crate::aviso!("sesión: stop() falló: {e}");
        }
        if let Err(e) = a.sesion.join() {
            crate::aviso!("sesión: join() falló: {e}");
        }
        cierres.fetch_add(1, Ordering::SeqCst);
        crate::info!("sesión: cerrada con stop()");
    }
}

fn hilo(rx: std_mpsc::Receiver<Cmd>, propio: std_mpsc::Sender<Cmd>, ev: mpsc::Sender<EventoSesion>, cierres: Arc<AtomicUsize>) {
    let log = nuevo_log();
    let mut activa: Option<Activa<'_>> = None;
    let mut numero = 0u64;
    while let Ok(c) = rx.recv() {
        match c {
            Cmd::Despertar(p) => {
                registrar_privado(&p);
                match p.credencial_despertar() {
                    Some(cred) => match Discovery::wakeup(&log, None, &p.host, cred, true) {
                        Ok(()) => crate::depurar!("sesión: paquete de encendido enviado"),
                        Err(e) => crate::aviso!("sesión: no pude mandar el paquete de encendido: {e}"),
                    },
                    None => crate::falla!("sesión: la registración no sirve para despertar la PS5"),
                }
            }
            Cmd::Conectar(p) => {
                registrar_privado(&p);
                cerrar(&mut activa, &cierres);
                numero += 1;
                match abrir(&log, &p, numero, &ev, &propio) {
                    Ok(a) => activa = Some(a),
                    Err(e) => {
                        crate::falla!("sesión: {e}");
                        avisar(&ev, EventoSesion::Cerrada { motivo: MotivoCierre::Otro });
                    }
                }
            }
            Cmd::Detener(listo) => {
                cerrar(&mut activa, &cierres);
                let _ = listo.send(());
            }
            Cmd::Fin(listo) => {
                cerrar(&mut activa, &cierres);
                let _ = listo.send(());
                return;
            }
            Cmd::Mando(e) => {
                if let Some(a) = activa.as_mut() {
                    let _ = a.sesion.set_controller_state(&a_chiaki(&e));
                }
            }
            Cmd::Keyframe => {
                if let Some(a) = activa.as_mut() {
                    match a.sesion.request_idr() {
                        Ok(()) => crate::depurar!("sesión: keyframe pedido"),
                        Err(e) => crate::aviso!("sesión: no pude pedir un keyframe: {e}"),
                    }
                }
            }
            Cmd::Reposo => {
                if let Some(a) = activa.as_mut() {
                    match a.sesion.goto_bed() {
                        Ok(()) => crate::info!("sesión: PS5 a reposo"),
                        Err(e) => crate::aviso!("sesión: no pude poner la PS5 en reposo: {e}"),
                    }
                }
            }
            Cmd::Terminada(n) => {
                // La PS5 cerró: se espera al hilo de libchiaki y se libera (el Drop hace stop + fini).
                if let Some(mut a) = activa.take_if(|a| a.numero == n) {
                    let _ = a.sesion.join();
                }
            }
            Cmd::FalloFec { sesion, pedido_enviado } => {
                if let Some(a) = activa.as_mut().filter(|a| a.numero == sesion) {
                    if pedido_enviado {
                        crate::depurar!("sesión: falla de FEC, libchiaki ya pidió el keyframe");
                    } else if a.sesion.request_idr().is_ok() {
                        // Igual que chiaki-ng: si el pedido de libchiaki no salió, se reintenta y se espera el keyframe.
                        a.sesion.video_receiver_set_waiting_for_idr(true);
                        crate::depurar!("sesión: falla de FEC, keyframe pedido de nuevo");
                    }
                }
            }
            Cmd::PidePin(n) => {
                if activa.as_ref().is_some_and(|a| a.numero == n) {
                    crate::falla!("sesión: la PS5 pide el PIN de inicio de sesión; hay que desactivarlo en la consola");
                    cerrar(&mut activa, &cierres);
                    avisar(&ev, EventoSesion::Cerrada { motivo: MotivoCierre::Otro });
                }
            }
        }
    }
    // No debería llegar acá (el hilo tiene su propio Sender), pero si llega, se cierra limpio igual.
    cerrar(&mut activa, &cierres);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesion::ConLimite;

    /// Una dirección de documentación (RFC 5737) que no responde: la sesión queda intentando conectar
    /// varios segundos, así hay una sesión abierta que cerrar.
    fn perfil_inalcanzable() -> Perfil {
        Perfil {
            host: "192.0.2.1".into(),
            regist_key: *b"a1b2c3d4\0\0\0\0\0\0\0\0",
            morning: [0; 16],
            account_id: [0; 8],
            bitrate_kbps: 8000,
        }
    }

    #[test]
    fn soltar_el_ultimo_handle_cierra_la_sesion_una_vez() {
        let cierres = Arc::new(AtomicUsize::new(0));
        let s = SesionLibchiaki::con_contador(cierres.clone()).unwrap();
        s.conectar(&perfil_inalcanzable()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        drop(s);
        assert_eq!(cierres.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn un_panico_en_la_tarea_duena_cierra_la_sesion_antes_de_salir() {
        let cierres = Arc::new(AtomicUsize::new(0));
        let c = cierres.clone();
        let r = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let s = Arc::new(ConLimite::new(SesionLibchiaki::con_contador(c).unwrap(), 3_000));
                s.conectar(&perfil_inalcanzable()).unwrap();
                tokio::time::sleep(Duration::from_millis(300)).await;
                panic!("prueba: pánico en el ciclo");
            })
        })
        .join();
        assert!(r.is_err(), "la tarea tenía que entrar en pánico");
        assert_eq!(cierres.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn detener_y_despues_soltar_cierra_una_sola_vez() {
        let cierres = Arc::new(AtomicUsize::new(0));
        let s = SesionLibchiaki::con_contador(cierres.clone()).unwrap();
        s.conectar(&perfil_inalcanzable()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(s.detener());
        assert_eq!(cierres.load(Ordering::SeqCst), 1);
        drop(s);
        assert_eq!(cierres.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cada_boton_va_a_su_boton_de_libchiaki() {
        let pares = [
            (mando::CRUZ, libchiaki::BUTTON_CROSS),
            (mando::CIRCULO, libchiaki::BUTTON_MOON),
            (mando::CUADRADO, libchiaki::BUTTON_BOX),
            (mando::TRIANGULO, libchiaki::BUTTON_PYRAMID),
            (mando::L1, libchiaki::BUTTON_L1),
            (mando::R1, libchiaki::BUTTON_R1),
            (mando::L3, libchiaki::BUTTON_L3),
            (mando::R3, libchiaki::BUTTON_R3),
            (mando::OPTIONS, libchiaki::BUTTON_OPTIONS),
            (mando::CREATE, libchiaki::BUTTON_SHARE),
            (mando::PS, libchiaki::BUTTON_PS),
            (mando::PANEL, libchiaki::BUTTON_TOUCHPAD),
            (mando::ARRIBA, libchiaki::BUTTON_DPAD_UP),
            (mando::ABAJO, libchiaki::BUTTON_DPAD_DOWN),
            (mando::IZQUIERDA, libchiaki::BUTTON_DPAD_LEFT),
            (mando::DERECHA, libchiaki::BUTTON_DPAD_RIGHT),
        ];
        for (nuestro, suyo) in pares {
            let s = a_chiaki(&EstadoMando { botones: nuestro, ..EstadoMando::suelto() });
            assert_eq!(s.0.buttons, suyo, "bit {nuestro:#x}");
        }
        // Los 16 botones de libchiaki quedan cubiertos, sin repetir.
        let todos = a_chiaki(&EstadoMando { botones: mando::CONOCIDOS, ..EstadoMando::suelto() });
        assert_eq!(todos.0.buttons, 0xFFFF);
    }

    #[test]
    fn el_orden_de_libchiaki_no_es_el_nuestro() {
        // Si se pasaran los bits tal cual, L1 llegaría como la flecha izquierda: por eso la tabla explícita.
        assert_eq!(mando::L1, libchiaki::BUTTON_DPAD_LEFT);
        assert_eq!(a_chiaki(&EstadoMando { botones: mando::L1, ..EstadoMando::suelto() }).0.buttons, libchiaki::BUTTON_L1);
    }

    #[test]
    fn bits_desconocidos_no_llegan_y_los_analogicos_pasan() {
        let e = EstadoMando { botones: 1 << 20 | mando::CRUZ, l2: 200, r2: 10, lx: -32768, ly: 32767, rx: 5, ry: -5 };
        let s = a_chiaki(&e);
        assert_eq!(s.0.buttons, libchiaki::BUTTON_CROSS);
        assert_eq!((s.0.l2_state, s.0.r2_state), (200, 10));
        assert_eq!((s.0.left_x, s.0.left_y, s.0.right_x, s.0.right_y), (-32768, 32767, 5, -5));
    }

    #[test]
    fn suelto_es_el_estado_ocioso() {
        assert!(a_chiaki(&EstadoMando::suelto()).equals(&ControllerState::idle()));
    }

    #[test]
    fn motivos_de_cierre() {
        use ffi::ChiakiQuitReason::*;
        assert_eq!(motivo(CHIAKI_QUIT_REASON_SESSION_REQUEST_RP_IN_USE), MotivoCierre::EnUso);
        assert_eq!(motivo(CHIAKI_QUIT_REASON_CTRL_CONNECT_FAILED), MotivoCierre::Red);
        assert_eq!(motivo(CHIAKI_QUIT_REASON_STREAM_CONNECTION_REMOTE_DISCONNECTED), MotivoCierre::Red);
        assert_eq!(motivo(CHIAKI_QUIT_REASON_SESSION_REQUEST_RP_VERSION_MISMATCH), MotivoCierre::Otro);
        assert_eq!(super::super::CODIGO_EN_USO, ffi::CHIAKI_RP_APPLICATION_REASON_IN_USE);
    }

    #[test]
    fn el_cierre_se_registra_con_el_motivo_crudo_de_libchiaki() {
        use ffi::ChiakiQuitReason::*;
        let r = CHIAKI_QUIT_REASON_STREAM_CONNECTION_REMOTE_DISCONNECTED;
        let d = describir_cierre(r, "Un texto de la PS5");
        assert!(d.contains("CHIAKI_QUIT_REASON_STREAM_CONNECTION_REMOTE_DISCONNECTED = 11"), "{d}");
        assert!(d.contains(&format!("\"{}\"", libchiaki::quit_reason_string(r))), "{d}");
        assert!(d.contains("texto de la PS5: \"Un texto de la PS5\""), "{d}");
        assert!(d.ends_with("motivo Red"), "{d}");

        let d = describir_cierre(CHIAKI_QUIT_REASON_SESSION_REQUEST_RP_IN_USE, "");
        assert!(d.contains("CHIAKI_QUIT_REASON_SESSION_REQUEST_RP_IN_USE = 4"), "{d}");
        assert!(d.contains("sin texto de la PS5"), "{d}");
        assert!(d.ends_with("motivo EnUso"), "{d}");

        let d = describir_cierre(CHIAKI_QUIT_REASON_CTRL_UNKNOWN, "");
        assert!(d.contains("CHIAKI_QUIT_REASON_CTRL_UNKNOWN = 7") && d.ends_with("motivo Otro"), "{d}");
    }

    #[test]
    fn el_texto_de_cierre_tapa_el_nombre_de_casa() {
        registrar_privado(&Perfil {
            host: "consola-privada.ejemplo.org".into(),
            regist_key: [0; 16],
            morning: [0; 16],
            account_id: [0; 8],
            bitrate_kbps: 1,
        });
        let r = ffi::ChiakiQuitReason::CHIAKI_QUIT_REASON_STREAM_CONNECTION_UNKNOWN;
        let d = describir_cierre(r, "sin ruta a consola-privada.ejemplo.org");
        assert!(d.contains("sin ruta a [casa]") && !d.contains("consola-privada"), "{d}");
    }

    #[test]
    fn el_registro_de_chiaki_tapa_el_nombre_de_casa() {
        registrar_privado(&Perfil {
            host: "casa.ejemplo.net".into(),
            regist_key: [0; 16],
            morning: [0; 16],
            account_id: [0; 8],
            bitrate_kbps: 1,
        });
        assert_eq!(ocultar("conectando a casa.ejemplo.net:9295"), "conectando a [casa]:9295");
        assert_eq!(ocultar("nada que tapar"), "nada que tapar");
    }
}

//! El ciclo de la sesión: despertar → conectar → transmitir y, si se cae, reintentar con espera
//! creciente (1, 2, 4… hasta 60 s). Si la PS5 dice "Remote is already in use", espera 30 s y reintenta
//! sin despertarla de nuevo. Mientras tanto la salida nunca se corta: muestra el cartel.
//! Ante SIGTERM cierra la sesión con `session.stop()` (vía `detener()`) y termina en menos de 5 s.
//! Recibe órdenes del servidor (`/control/*`): reconectar, reposo (la PS5 se apaga y el ciclo queda en
//! pausa, si no la volvería a despertar) y encender (sale de la pausa). Escribe las métricas de la salud.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::config::Perfil;
use crate::metricas::{EstadoSesion, Metricas};
use crate::mux::Destino;
use crate::salida::Salida;
use crate::sesion::{EventoSesion, MotivoCierre, SesionPs5};
use tokio::sync::mpsc;
use tokio::time::Instant;

pub const ESPERA_INICIAL: Duration = Duration::from_secs(1);
pub const ESPERA_MAXIMA: Duration = Duration::from_secs(60);
pub const ESPERA_EN_USO: Duration = Duration::from_secs(30);
/// Si después de `conectar` no llega ni `Conectada` ni `Cerrada`, se da el intento por fallido.
pub const PLAZO_CONEXION: Duration = Duration::from_secs(20);
/// Conectada pero sin video por este tiempo: la sesión se da por caída (por ejemplo, si se perdió el aviso).
pub const SIN_VIDEO: Duration = Duration::from_secs(10);
/// Una sesión que duró al menos esto vuelve la espera a 1 s; si se cae antes, la espera sigue creciendo.
pub const SESION_ESTABLE: Duration = Duration::from_secs(30);
/// Cuánto se espera al cierre limpio antes de salir igual (el total queda debajo de 5 s).
pub const PLAZO_DETENER: Duration = Duration::from_secs(4);
/// Después de mandar la PS5 a reposo, si la sesión no terminó sola en este tiempo, se cierra con `stop()`
/// (si quedara abierta, la próxima conexión daría "Remote is already in use").
pub const PLAZO_REPOSO: Duration = Duration::from_secs(10);

/// Órdenes del servidor (`POST /control/{reconectar,reposo,encender}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orden {
    /// Cierra la sesión (si hay) y reintenta enseguida. En reposo no hace nada: para eso está `Encender`.
    Reconectar,
    /// Pone la PS5 en reposo (si hay sesión) y deja el ciclo en pausa.
    Reposo,
    /// Sale de la pausa (despertar + conectar ya). Si el ciclo estaba esperando, adelanta el intento.
    Encender,
}

/// Lo que comparten el ciclo y el servidor.
pub struct Enlace {
    pub ordenes: mpsc::Receiver<Orden>,
    pub metricas: Arc<Metricas>,
}

impl Enlace {
    /// El enlace y el `Sender` de órdenes que se le pasa al servidor.
    pub fn nuevo(metricas: Arc<Metricas>) -> (mpsc::Sender<Orden>, Enlace) {
        let (tx, ordenes) = mpsc::channel(16);
        (tx, Enlace { ordenes, metricas })
    }
}

enum Estado {
    Esperando { hasta: Instant, despertar: bool },
    Conectando { desde: Instant },
    Conectada { desde: Instant, ultimo_video: Instant },
    /// En pausa por `Orden::Reposo`: no se despierta ni se conecta hasta `Orden::Encender`.
    /// `cerrar_en`: cuándo cerrar con `stop()` la sesión, por si la PS5 no la terminó al dormirse.
    Reposo { cerrar_en: Option<Instant> },
}

impl Estado {
    fn plazo(&self) -> Option<Instant> {
        match *self {
            Estado::Esperando { hasta, .. } => Some(hasta),
            Estado::Conectando { desde } => Some(desde + PLAZO_CONEXION),
            Estado::Conectada { ultimo_video, .. } => Some(ultimo_video + SIN_VIDEO),
            Estado::Reposo { cerrar_en } => cerrar_en,
        }
    }

    fn para_metricas(&self) -> EstadoSesion {
        match self {
            Estado::Conectada { .. } => EstadoSesion::Conectada,
            Estado::Conectando { .. } => EstadoSesion::Conectando,
            Estado::Esperando { .. } => EstadoSesion::Caida,
            Estado::Reposo { .. } => EstadoSesion::Reposo,
        }
    }
}

/// Pasa a las métricas el estado del ciclo si cambió (y la hora de la conexión, si se conectó).
fn sincronizar(metricas: &Metricas, estado: &Estado, anterior: &mut EstadoSesion) {
    let nuevo = estado.para_metricas();
    if nuevo != *anterior {
        if nuevo == EstadoSesion::Conectada {
            metricas.conectada(Instant::now(), SystemTime::now());
        } else {
            metricas.estado(nuevo);
        }
        *anterior = nuevo;
    }
}

/// Corre hasta que `senales` se complete (SIGTERM o SIGINT en `main`).
pub async fn correr<S, D, F>(sesion: Arc<S>, perfil: Perfil, mut salida: Salida<D>, senales: F, enlace: Enlace)
where
    S: SesionPs5,
    D: Destino,
    F: Future<Output = ()>,
{
    let t0 = Instant::now();
    let Enlace { mut ordenes, metricas } = enlace;
    let mut ordenes_abiertas = true;
    let mut en_metricas = EstadoSesion::Caida;
    metricas.estado(en_metricas);
    let Some(mut eventos) = sesion.eventos() else {
        crate::falla!("ciclo: el canal de eventos de la sesión ya estaba tomado");
        return;
    };
    tokio::pin!(senales);
    let mut estado = Estado::Esperando { hasta: t0, despertar: true };
    let mut espera = ESPERA_INICIAL;
    let mut intento = 0u64;

    // Pasa a esperar el próximo intento; con EnUso, 30 s fijos y sin despertar.
    let reintentar = |espera: &mut Duration, en_uso: bool| -> Estado {
        let ahora = Instant::now();
        if en_uso {
            crate::info!("ciclo: la PS5 tiene otra sesión abierta; reintento en {} s sin despertarla", ESPERA_EN_USO.as_secs());
            Estado::Esperando { hasta: ahora + ESPERA_EN_USO, despertar: false }
        } else {
            let e = *espera;
            *espera = (e * 2).min(ESPERA_MAXIMA);
            crate::info!("ciclo: reintento en {} s", e.as_secs());
            Estado::Esperando { hasta: ahora + e, despertar: true }
        }
    };

    loop {
        let plazo = estado.plazo();
        let proximo_cuadro = t0 + salida.proximo();
        sincronizar(&metricas, &estado, &mut en_metricas);
        tokio::select! {
            biased;
            _ = &mut senales => {
                crate::info!("ciclo: pedido de apagado; cierro la sesión");
                if tokio::time::timeout(PLAZO_DETENER, sesion.detener()).await.is_err() {
                    crate::aviso!("ciclo: la sesión no cerró en {} s; salgo igual", PLAZO_DETENER.as_secs());
                }
                return;
            }
            ev = eventos.recv() => {
                let Some(ev) = ev else {
                    crate::falla!("ciclo: se cerró el canal de eventos de la sesión");
                    let _ = tokio::time::timeout(PLAZO_DETENER, sesion.detener()).await;
                    return;
                };
                let ahora = Instant::now();
                match ev {
                    EventoSesion::Conectada => {
                        if let Estado::Conectando { desde } = estado {
                            crate::info!("ciclo: sesión conectada (intento {intento}, {} ms)", (ahora - desde).as_millis());
                            estado = Estado::Conectada { desde: ahora, ultimo_video: ahora };
                        }
                    }
                    EventoSesion::Video { datos, perdidos } => {
                        // libchiaki puede entregar el primer cuadro (el SPS/PPS) antes del aviso de
                        // conectada: el video cuenta como conexión, así el encabezado no se pierde.
                        if let Estado::Conectando { desde } = estado {
                            crate::info!("ciclo: sesión conectada (intento {intento}, {} ms; llegó video)", (ahora - desde).as_millis());
                            estado = Estado::Conectada { desde: ahora, ultimo_video: ahora };
                        }
                        if let Estado::Conectada { ref mut ultimo_video, .. } = estado {
                            *ultimo_video = ahora;
                            sincronizar(&metricas, &estado, &mut en_metricas);
                            metricas.cuadro(ahora, datos.len(), perdidos);
                            salida.video_ps5(&datos, perdidos, t0.elapsed());
                        }
                    }
                    EventoSesion::Audio { datos } => {
                        if let Estado::Conectada { .. } = estado {
                            salida.audio_ps5(&datos, t0.elapsed());
                        }
                    }
                    EventoSesion::FormatoAudio { frecuencia, muestras_por_paquete, .. } => {
                        salida.formato_audio(frecuencia, muestras_por_paquete);
                    }
                    EventoSesion::Cerrada { motivo } => {
                        let duro = match estado {
                            Estado::Conectada { desde, .. } => Some(ahora - desde),
                            Estado::Conectando { .. } => Some(Duration::ZERO),
                            // En reposo la PS5 cierra la sesión al dormirse: no se reintenta.
                            Estado::Esperando { .. } | Estado::Reposo { .. } => None,
                        };
                        if let Some(duro) = duro {
                            crate::info!("ciclo: la sesión terminó ({motivo:?}) después de {} s", duro.as_secs());
                            salida.sesion_caida(t0.elapsed());
                            if duro >= SESION_ESTABLE {
                                espera = ESPERA_INICIAL;
                            }
                            estado = reintentar(&mut espera, motivo == MotivoCierre::EnUso);
                        }
                    }
                }
            }
            orden = ordenes.recv(), if ordenes_abiertas => {
                let Some(orden) = orden else {
                    ordenes_abiertas = false;
                    continue;
                };
                let ahora = Instant::now();
                match (orden, &estado) {
                    (Orden::Reposo, Estado::Reposo { .. }) => crate::info!("ciclo: reposo pedido, pero ya estaba en reposo"),
                    (Orden::Reposo, _) => {
                        sesion.reposo();
                        estado = if let Estado::Conectada { .. } = estado {
                            crate::info!("ciclo: PS5 a reposo; el ciclo queda en pausa hasta encender");
                            Estado::Reposo { cerrar_en: Some(ahora + PLAZO_REPOSO) }
                        } else {
                            crate::aviso!("ciclo: reposo pedido sin sesión (la PS5 no se enteró); el ciclo queda en pausa hasta encender");
                            if let Estado::Conectando { .. } = estado {
                                tokio::spawn(sesion.detener());
                            }
                            Estado::Reposo { cerrar_en: None }
                        };
                        salida.sesion_caida(t0.elapsed());
                    }
                    (Orden::Encender, Estado::Reposo { .. } | Estado::Esperando { .. }) => {
                        crate::info!("ciclo: encender pedido; despierto la PS5 y conecto");
                        espera = ESPERA_INICIAL;
                        estado = Estado::Esperando { hasta: ahora, despertar: true };
                    }
                    (Orden::Encender, _) => crate::info!("ciclo: encender pedido, pero ya hay sesión"),
                    (Orden::Reconectar, Estado::Reposo { .. }) => {
                        crate::aviso!("ciclo: reconectar pedido en reposo; no hago nada (para salir de la pausa, encender)");
                    }
                    (Orden::Reconectar, Estado::Esperando { despertar, .. }) => {
                        crate::info!("ciclo: reconectar pedido; adelanto el intento");
                        espera = ESPERA_INICIAL;
                        estado = Estado::Esperando { hasta: ahora, despertar: *despertar };
                    }
                    (Orden::Reconectar, Estado::Conectando { .. } | Estado::Conectada { .. }) => {
                        crate::info!("ciclo: reconectar pedido; cierro la sesión y reintento en {} s", ESPERA_INICIAL.as_secs());
                        // Igual que al darla por caída: se cierra en segundo plano para no frenar el cartel.
                        tokio::spawn(sesion.detener());
                        salida.sesion_caida(t0.elapsed());
                        espera = ESPERA_INICIAL;
                        estado = Estado::Esperando { hasta: ahora + ESPERA_INICIAL, despertar: true };
                    }
                }
            }
            _ = tokio::time::sleep_until(plazo.unwrap_or(t0)), if plazo.is_some() => {
                match estado {
                    Estado::Esperando { despertar, .. } => {
                        intento += 1;
                        if despertar {
                            if let Err(e) = sesion.despertar(&perfil) {
                                crate::falla!("ciclo: no pude despertar la PS5: {e}");
                            }
                        }
                        estado = match sesion.conectar(&perfil) {
                            Ok(()) => {
                                crate::info!("ciclo: conectando (intento {intento})");
                                Estado::Conectando { desde: Instant::now() }
                            }
                            Err(e) => {
                                crate::falla!("ciclo: no pude iniciar la sesión: {e}");
                                reintentar(&mut espera, false)
                            }
                        };
                    }
                    Estado::Conectando { .. } | Estado::Conectada { .. } => {
                        let que = if let Estado::Conectando { .. } = estado { "la PS5 no respondió" } else { "no llega video" };
                        crate::aviso!("ciclo: {que}; cierro el intento y reintento");
                        // Igual que con Cerrada: si la sesión venía estable (un corte de Wi-Fi), se reintenta enseguida.
                        if let Estado::Conectada { desde, .. } = estado {
                            if Instant::now() - desde >= SESION_ESTABLE {
                                espera = ESPERA_INICIAL;
                            }
                        }
                        // Se cierra en segundo plano para no frenar el cartel.
                        tokio::spawn(sesion.detener());
                        salida.sesion_caida(t0.elapsed());
                        estado = reintentar(&mut espera, false);
                    }
                    Estado::Reposo { .. } => {
                        crate::info!("ciclo: cierro la sesión que quedó del reposo");
                        tokio::spawn(sesion.detener());
                        estado = Estado::Reposo { cerrar_en: None };
                    }
                }
            }
            _ = tokio::time::sleep_until(proximo_cuadro) => {
                salida.tick(t0.elapsed());
            }
        }
        sincronizar(&metricas, &estado, &mut en_metricas);
        // Mientras se muestra el cartel con la sesión arriba, se pide un keyframe (el limitador deja uno cada 3 s).
        if let Estado::Conectada { .. } = estado {
            if salida.en_cartel() && sesion.pedir_keyframe() {
                crate::depurar!("ciclo: pedí un keyframe para empalmar");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::tests::leer_pes;
    use crate::mux::ID_VIDEO;
    use crate::salida::tests::cartel_de_prueba;
    use crate::sesion::falsa::SesionFalsa;
    use crate::sesion::ConLimite;
    use std::sync::Mutex;
    use tokio::sync::oneshot;
    use tokio::time::sleep;

    /// Destino que guarda lo emitido donde el test lo puede leer.
    #[derive(Clone, Default)]
    struct Compartido(Arc<Mutex<Vec<u8>>>);

    impl Destino for Compartido {
        fn enviar(&mut self, datos: &[u8]) {
            self.0.lock().unwrap().extend_from_slice(datos);
        }
    }

    fn perfil() -> Perfil {
        Perfil {
            host: "consola.ejemplo.net".into(),
            regist_key: *b"a1b2c3d4\0\0\0\0\0\0\0\0",
            morning: [0; 16],
            account_id: [0; 8],
            bitrate_kbps: 8000,
        }
    }

    struct Banco {
        sesion: Arc<ConLimite<SesionFalsa>>,
        ordenes: mpsc::Sender<Orden>,
        metricas: Arc<Metricas>,
        salida: Compartido,
        terminar: Option<oneshot::Sender<()>>,
        tarea: tokio::task::JoinHandle<()>,
        t0: Instant,
    }

    fn arrancar(guion: Vec<Vec<EventoSesion>>) -> Banco {
        let sesion = Arc::new(ConLimite::new(SesionFalsa::con_guion(guion), 3_000));
        let salida = Compartido::default();
        let (tx, rx) = oneshot::channel::<()>();
        let metricas = Arc::new(Metricas::new());
        let (ordenes, enlace) = Enlace::nuevo(metricas.clone());
        let tarea = tokio::spawn(correr(
            sesion.clone(),
            perfil(),
            Salida::new(cartel_de_prueba(), salida.clone()),
            async move {
                let _ = rx.await;
            },
            enlace,
        ));
        Banco { sesion, ordenes, metricas, salida, terminar: Some(tx), tarea, t0: Instant::now() }
    }

    impl Banco {
        fn registro(&self) -> crate::sesion::falsa::Registro {
            self.sesion.interna().registro()
        }
        /// Segundos desde el arranque de cada llamada.
        fn segundos(&self, v: &[Instant]) -> Vec<f64> {
            v.iter().map(|t| (*t - self.t0).as_secs_f64()).collect()
        }
        async fn terminar(mut self) -> (Self, Duration) {
            let t = Instant::now();
            self.terminar.take().unwrap().send(()).unwrap();
            (&mut self.tarea).await.unwrap();
            (self, t.elapsed())
        }
        fn videos(&self) -> Vec<(u64, Vec<u8>)> {
            leer_pes(&self.salida.0.lock().unwrap())
                .into_iter()
                .filter(|p| p.0 == ID_VIDEO)
                .map(|p| (p.1, p.2))
                .collect()
        }
        fn emisor(&self) -> tokio::sync::mpsc::Sender<EventoSesion> {
            self.sesion.interna().emisor()
        }
        async fn orden(&self, o: Orden) {
            self.ordenes.send(o).await.unwrap();
            sleep(Duration::from_millis(1)).await;
        }
        fn estado(&self) -> EstadoSesion {
            self.metricas.foto(Instant::now()).estado
        }
    }

    fn diferencias(v: &[f64]) -> Vec<f64> {
        v.windows(2).map(|w| ((w[1] - w[0]) * 1000.0).round() / 1000.0).collect()
    }

    fn video(datos: Vec<u8>) -> EventoSesion {
        EventoSesion::Video { datos, perdidos: 0 }
    }

    #[tokio::test(start_paused = true)]
    async fn conexion_ok_transmite_y_al_caerse_vuelve_el_cartel() {
        let b = arrancar(vec![vec![EventoSesion::Conectada]]);
        sleep(Duration::from_millis(500)).await;
        let tx = b.emisor();
        tx.send(video(vec![0, 0, 0, 1, 0x65, 0x50, 0])).await.unwrap();
        for k in 1..60u8 {
            sleep(Duration::from_micros(16_667)).await;
            tx.send(video(vec![0, 0, 0, 1, 0x41, 0x50, k])).await.unwrap();
        }
        sleep(Duration::from_millis(10)).await;
        let r = b.registro();
        assert_eq!(r.conectar.len(), 1);
        assert_eq!(r.despertar.len(), 1, "despierta antes de conectar");
        assert!(r.keyframes >= 1, "pide un keyframe al conectar");
        let v = b.videos();
        let ps5 = v.iter().filter(|c| c.1.contains(&0x50)).count();
        assert_eq!(ps5, 60, "los 60 cuadros de la PS5 salieron");
        assert!(v.last().unwrap().1.contains(&0x50));

        tx.send(EventoSesion::Cerrada { motivo: MotivoCierre::Red }).await.unwrap();
        sleep(Duration::from_millis(100)).await;
        assert!(b.videos().last().unwrap().1.contains(&0xCA), "volvió el cartel");
        let pts: Vec<u64> = b.videos().iter().map(|c| c.0).collect();
        assert!(pts.windows(2).all(|w| w[1] > w[0]));
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn sin_sesion_el_cartel_sale_enseguida() {
        let b = arrancar(vec![]);
        sleep(Duration::from_millis(100)).await;
        let v = b.videos();
        assert!(v.len() >= 6, "{}", v.len());
        assert!(v.iter().all(|c| c.1.contains(&0xCA)));
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn caida_por_red_reintenta_con_espera_creciente_hasta_60_s() {
        let cerrada = vec![EventoSesion::Cerrada { motivo: MotivoCierre::Red }];
        let b = arrancar(vec![cerrada; 9]);
        sleep(Duration::from_secs(190)).await;
        let r = b.registro();
        let t = b.segundos(&r.conectar);
        assert!(t.len() >= 9, "{t:?}");
        assert_eq!(diferencias(&t[..9]), vec![1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 60.0, 60.0]);
        assert_eq!(t[0], 0.0, "el primer intento es inmediato");
        // Con Red se despierta antes de cada intento.
        assert_eq!(b.segundos(&r.despertar)[..9], t[..9]);
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn en_uso_espera_30_s_y_no_vuelve_a_despertar() {
        let en_uso = vec![EventoSesion::Cerrada { motivo: MotivoCierre::EnUso }];
        let b = arrancar(vec![en_uso; 3]);
        sleep(Duration::from_secs(95)).await;
        let r = b.registro();
        let t = b.segundos(&r.conectar);
        assert_eq!(t, vec![0.0, 30.0, 60.0, 90.0]);
        assert_eq!(r.despertar.len(), 1, "solo el despertar del primer intento");
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn sigterm_detiene_la_sesion_una_vez_y_sale_rapido() {
        let b = arrancar(vec![vec![EventoSesion::Conectada]]);
        sleep(Duration::from_secs(2)).await;
        let (b, tardo) = b.terminar().await;
        assert_eq!(b.registro().detener, 1);
        assert!(tardo < Duration::from_secs(5), "{tardo:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn sigterm_sin_sesion_tambien_detiene_una_vez() {
        let b = arrancar(vec![vec![EventoSesion::Cerrada { motivo: MotivoCierre::Red }]]);
        sleep(Duration::from_millis(1_500)).await;
        let (b, _) = b.terminar().await;
        assert_eq!(b.registro().detener, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn si_no_responde_al_conectar_se_detiene_y_reintenta() {
        let b = arrancar(vec![]);
        sleep(Duration::from_secs(22)).await;
        let r = b.registro();
        assert_eq!(b.segundos(&r.conectar), vec![0.0, 21.0], "plazo de 20 s y espera de 1 s");
        assert_eq!(r.detener, 1, "cierra el intento colgado");
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn una_sesion_estable_vuelve_la_espera_a_1_s() {
        let red = || vec![EventoSesion::Cerrada { motivo: MotivoCierre::Red }];
        let b = arrancar(vec![red(), red(), vec![EventoSesion::Conectada], red()]);
        sleep(Duration::from_secs(3)).await; // intentos en 0, 1 y 3 s; el de 3 s conecta
        let tx = b.emisor();
        for _ in 0..8 {
            sleep(Duration::from_secs(5)).await;
            tx.send(video(vec![0, 0, 0, 1, 0x41, 0x50, 0])).await.unwrap();
        }
        tx.send(EventoSesion::Cerrada { motivo: MotivoCierre::Red }).await.unwrap(); // a los 43 s
        sleep(Duration::from_secs(5)).await;
        let t = b.segundos(&b.registro().conectar);
        assert_eq!(t, vec![0.0, 1.0, 3.0, 44.0, 46.0], "después de 40 s conectada, la espera vuelve a 1 s");
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn una_sesion_estable_que_se_queda_sin_video_reintenta_en_1_s() {
        let red = || vec![EventoSesion::Cerrada { motivo: MotivoCierre::Red }];
        let b = arrancar(vec![red(), red(), vec![EventoSesion::Conectada], vec![]]);
        sleep(Duration::from_secs(3)).await; // intentos en 0, 1 y 3 s (la espera ya va en 4 s); el de 3 s conecta
        let tx = b.emisor();
        for _ in 0..8 {
            sleep(Duration::from_secs(5)).await;
            tx.send(video(vec![0, 0, 0, 1, 0x41, 0x50, 0])).await.unwrap();
        }
        // Último cuadro a los 43 s: a los 53 s se da por caída (sin Cerrada, como un corte de Wi-Fi).
        sleep(Duration::from_secs(15)).await;
        let t = b.segundos(&b.registro().conectar);
        assert_eq!(t, vec![0.0, 1.0, 3.0, 54.0], "40 s conectada: la espera vuelve a 1 s también sin Cerrada");
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn el_encabezado_que_llega_antes_de_conectada_no_se_pierde() {
        let sps_pps = vec![0, 0, 0, 1, 0x67, 0x50, 0x51, 0, 0, 1, 0x68, 0x52];
        let idr = vec![0, 0, 0, 1, 0x65, 0x50, 1];
        // libchiaki puede entregar el encabezado (SPS/PPS) antes del evento de conectada.
        let b = arrancar(vec![vec![video(sps_pps.clone()), EventoSesion::Conectada, video(idr.clone())]]);
        sleep(Duration::from_millis(100)).await;
        let v = b.videos();
        let primero = &v.iter().find(|c| c.1.contains(&0x50)).expect("salió la PS5").1;
        assert_eq!(primero, &[sps_pps, idr].concat(), "el keyframe lleva el SPS/PPS de la PS5, no el del cartel");
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn conectada_sin_video_por_10_s_se_da_por_caida() {
        let b = arrancar(vec![vec![EventoSesion::Conectada], vec![EventoSesion::Conectada]]);
        sleep(Duration::from_secs(12)).await;
        let r = b.registro();
        assert_eq!(b.segundos(&r.conectar), vec![0.0, 11.0]);
        assert_eq!(r.detener, 1);
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn reposo_con_sesion_apaga_la_ps5_y_pausa_el_ciclo_hasta_encender() {
        let b = arrancar(vec![vec![EventoSesion::Conectada], vec![EventoSesion::Conectada]]);
        sleep(Duration::from_secs(1)).await;
        b.emisor().send(video(vec![0, 0, 0, 1, 0x65, 0x50, 0])).await.unwrap();
        b.orden(Orden::Reposo).await;
        assert_eq!(b.registro().reposo, 1, "manda la PS5 a reposo");
        assert_eq!(b.estado(), EstadoSesion::Reposo, "la salud dice reposo, no caída");
        sleep(Duration::from_millis(50)).await;
        assert!(b.videos().last().unwrap().1.contains(&0xCA), "vuelve el cartel");
        // La PS5 cierra la sesión al irse a dormir: el ciclo no la vuelve a despertar.
        b.emisor().send(EventoSesion::Cerrada { motivo: MotivoCierre::Red }).await.unwrap();
        sleep(Duration::from_secs(120)).await;
        let r = b.registro();
        assert_eq!((r.despertar.len(), r.conectar.len()), (1, 1), "en pausa: ni despierta ni conecta");
        assert_eq!(r.detener, 1, "a los 10 s cierra con stop() por si la sesión quedó abierta");
        assert_eq!(b.estado(), EstadoSesion::Reposo, "sigue en reposo hasta encender");
        b.orden(Orden::Encender).await;
        let r = b.registro();
        assert_eq!((r.despertar.len(), r.conectar.len()), (2, 2), "encender: despierta y conecta ya");
        assert!((b.segundos(&r.conectar)[1] - 121.0).abs() < 0.1, "{:?}", b.segundos(&r.conectar));
        assert_eq!(b.estado(), EstadoSesion::Conectada);
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn reposo_sin_sesion_tambien_pausa_el_ciclo() {
        let red = || vec![EventoSesion::Cerrada { motivo: MotivoCierre::Red }];
        let b = arrancar(vec![red(), red(), red()]);
        sleep(Duration::from_millis(500)).await; // el primer intento falló; el próximo sería a 1 s
        b.orden(Orden::Reposo).await;
        assert_eq!(b.registro().reposo, 1, "se pide igual (sin sesión, la falsa lo anota)");
        assert_eq!(b.estado(), EstadoSesion::Reposo);
        sleep(Duration::from_secs(100)).await;
        assert_eq!(b.registro().conectar.len(), 1);
        // Reconectar no saca de la pausa.
        b.orden(Orden::Reconectar).await;
        sleep(Duration::from_secs(5)).await;
        assert_eq!(b.registro().conectar.len(), 1);
        assert_eq!(b.estado(), EstadoSesion::Reposo, "reconectar no saca del reposo");
        b.orden(Orden::Encender).await;
        assert_eq!(b.registro().conectar.len(), 2);
        assert_ne!(b.estado(), EstadoSesion::Reposo, "encender sale del reposo");
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn reconectar_cierra_la_sesion_y_reintenta_en_1_s() {
        let b = arrancar(vec![vec![EventoSesion::Conectada], vec![EventoSesion::Conectada]]);
        sleep(Duration::from_secs(2)).await;
        b.orden(Orden::Reconectar).await;
        assert_eq!(b.registro().detener, 1);
        assert_eq!(b.estado(), EstadoSesion::Caida);
        sleep(Duration::from_millis(1_100)).await;
        let r = b.registro();
        let t = b.segundos(&r.conectar);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!((t[1] - 3.0).abs() < 0.01, "{t:?}");
        assert_eq!(r.despertar.len(), 2);
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn encender_mientras_espera_adelanta_el_intento() {
        let red = || vec![EventoSesion::Cerrada { motivo: MotivoCierre::Red }];
        let b = arrancar(vec![red(), red(), red(), red()]);
        sleep(Duration::from_millis(1_500)).await; // intentos en 0 y 1 s; el próximo sería a los 3 s
        b.orden(Orden::Encender).await;
        let t = b.segundos(&b.registro().conectar);
        assert_eq!(t.len(), 3, "{t:?}");
        assert!((t[2] - 1.5).abs() < 0.01, "{t:?}");
        b.terminar().await;
    }

    #[tokio::test(start_paused = true)]
    async fn el_ciclo_escribe_las_metricas() {
        let b = arrancar(vec![vec![], vec![EventoSesion::Conectada]]);
        sleep(Duration::from_millis(10)).await;
        assert_eq!(b.estado(), EstadoSesion::Conectando);
        assert_eq!(b.metricas.foto(Instant::now()).ultima_reconexion, None);
        sleep(Duration::from_secs(21)).await; // el primero no responde: a los 20 s se reintenta (21 s)
        let tx = b.emisor();
        tx.send(EventoSesion::Video { datos: vec![0, 0, 0, 1, 0x65, 0x50, 0], perdidos: 0 }).await.unwrap();
        for k in 1..300u32 {
            sleep(Duration::from_micros(16_667)).await;
            tx.send(EventoSesion::Video { datos: vec![0, 0, 0, 1, 0x41, 0x50, 0, 0, 0, 0], perdidos: u32::from(k % 100 == 0) }).await.unwrap();
        }
        sleep(Duration::from_millis(1)).await;
        let f = b.metricas.foto(Instant::now());
        assert_eq!(f.estado, EstadoSesion::Conectada);
        // La ventana cuenta desde la conexión (21 s), 10 ms antes del primer cuadro: ~59 fps.
        assert!((f.fps - 60.0).abs() < 2.0, "{}", f.fps);
        assert_eq!(f.perdidos, 2);
        assert!(f.ultima_reconexion.is_some());
        tx.send(EventoSesion::Cerrada { motivo: MotivoCierre::Red }).await.unwrap();
        sleep(Duration::from_millis(1)).await;
        assert_eq!(b.estado(), EstadoSesion::Caida);
        b.terminar().await;
    }
}

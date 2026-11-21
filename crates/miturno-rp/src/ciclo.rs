//! El ciclo de la sesión: despertar → conectar → transmitir y, si se cae, reintentar con espera
//! creciente (1, 2, 4… hasta 60 s). Si la PS5 dice "Remote is already in use", espera 30 s y reintenta
//! sin despertarla de nuevo. Mientras tanto la salida nunca se corta: muestra el cartel.
//! Ante SIGTERM cierra la sesión con `session.stop()` (vía `detener()`) y termina en menos de 5 s.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::config::Perfil;
use crate::mux::Destino;
use crate::salida::Salida;
use crate::sesion::{EventoSesion, MotivoCierre, SesionPs5};
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

enum Estado {
    Esperando { hasta: Instant, despertar: bool },
    Conectando { desde: Instant },
    Conectada { desde: Instant, ultimo_video: Instant },
}

impl Estado {
    fn plazo(&self) -> Instant {
        match *self {
            Estado::Esperando { hasta, .. } => hasta,
            Estado::Conectando { desde } => desde + PLAZO_CONEXION,
            Estado::Conectada { ultimo_video, .. } => ultimo_video + SIN_VIDEO,
        }
    }
}

/// Corre hasta que `senales` se complete (SIGTERM o SIGINT en `main`).
pub async fn correr<S, D, F>(sesion: Arc<S>, perfil: Perfil, mut salida: Salida<D>, senales: F)
where
    S: SesionPs5,
    D: Destino,
    F: Future<Output = ()>,
{
    let t0 = Instant::now();
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
                            Estado::Esperando { .. } => None,
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
            _ = tokio::time::sleep_until(plazo) => {
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
                }
            }
            _ = tokio::time::sleep_until(proximo_cuadro) => {
                salida.tick(t0.elapsed());
            }
        }
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
        salida: Compartido,
        terminar: Option<oneshot::Sender<()>>,
        tarea: tokio::task::JoinHandle<()>,
        t0: Instant,
    }

    fn arrancar(guion: Vec<Vec<EventoSesion>>) -> Banco {
        let sesion = Arc::new(ConLimite::new(SesionFalsa::con_guion(guion), 3_000));
        let salida = Compartido::default();
        let (tx, rx) = oneshot::channel::<()>();
        let tarea = tokio::spawn(correr(
            sesion.clone(),
            perfil(),
            Salida::new(cartel_de_prueba(), salida.clone()),
            async move {
                let _ = rx.await;
            },
        ));
        Banco { sesion, salida, terminar: Some(tx), tarea, t0: Instant::now() }
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
}

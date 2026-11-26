//! miturno-rp: mantiene la sesión con la PS5 y saca su video sin recomprimir como MPEG-TS por UDP.
//! Sin sesión, la salida muestra el cartel "Volvemos enseguida" para que nunca se corte.
//! El servidor local (`/ctl`, `/interno/*`, `/control/*`) corre en el mismo runtime de varios hilos que
//! el ciclo: el ciclo (y el mux) es el futuro de `block_on` y corre en el hilo principal; lo lanzado con
//! `spawn` (el servidor y el muestreo de CPU) corre en los dos hilos de trabajo, que además atienden los
//! drivers de tiempo y de E/S de todo el runtime, también los del ciclo. Así una ráfaga en /ctl o de
//! pedidos de salud no le quita CPU al ciclo, pero el aislamiento no es total: depende de que ninguna
//! tarea de los hilos de trabajo los bloquee (hoy ninguna lo hace). Si alguna bloqueara, se demorarían
//! también los timers y los sockets del ciclo.

use std::process::ExitCode;
use std::sync::Arc;

use axum::serve::ListenerExt;
use miturno_rp::cartel::Cartel;
use miturno_rp::config::{self, Argumentos, Rutas};
use miturno_rp::metricas::Metricas;
use miturno_rp::mux::DestinoUdp;
use miturno_rp::procedencia::{self, Emisor};
use miturno_rp::salida::{self, Salida};
use miturno_rp::servidor::{self, EstadoServidor, Piezas};
use miturno_rp::sesion::libchiaki::SesionLibchiaki;
use miturno_rp::sesion::{ConLimite, MIN_ENTRE_KEYFRAMES_MS};
use miturno_rp::sistema::SistemaReal;
use miturno_rp::{ciclo, falla, info};
use tokio::signal::unix::{signal, SignalKind};

fn main() -> ExitCode {
    let argumentos: Vec<_> = std::env::args().skip(1).collect();
    if argumentos.first().is_some_and(|a| a == "--procedencia-sintetica") {
        return match procedencia::producir_sintetico(&argumentos[1..], &mut std::io::stdout().lock()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                falla!("{e}");
                ExitCode::FAILURE
            }
        };
    }
    let rutas = match config::parsear_argumentos(argumentos) {
        Ok(Argumentos::Version) => {
            println!("miturno-rp {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(Argumentos::Correr(r)) => r,
        Err(e) => {
            falla!("{e}");
            return ExitCode::from(2);
        }
    };
    match correr(rutas) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            falla!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn correr(rutas: Rutas) -> Result<(), String> {
    let config = config::cargar_config(&rutas.config).map_err(|e| e.to_string())?;
    let perfil = config::cargar_perfil(&rutas.casa, &rutas.ps5, config.bitrate_kbps).map_err(|e| e.to_string())?;
    let ts = std::fs::read(&config.cartel).map_err(|e| format!("no pude leer el cartel {}: {e}", config.cartel.display()))?;
    let cartel = Cartel::desde_ts(&ts).map_err(|e| format!("cartel {}: {e}", config.cartel.display()))?;
    let dir = config.salida_udp.parse().expect("validada al cargar la configuración");
    let destino = DestinoUdp::new(dir).map_err(|e| format!("no pude abrir el socket UDP de salida: {e}"))?;
    // La clave y el token se leen una sola vez, antes de arrancar nada: si faltan o están mal, se sale ya.
    let clave = servidor::cargar_clave_publica(&config.clave_publica)?;
    let token_control = servidor::cargar_token_control(&config.token_control)?;
    let http: std::net::SocketAddr = config.http.parse().expect("validada al cargar la configuración");
    let procedencia = match Emisor::desde_systemd() {
        Ok(emisor) => emisor,
        Err(e) => {
            miturno_rp::aviso!("salida: procedencia desactivada: {e}");
            None
        }
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("no pude crear el runtime: {e}"))?;
    rt.block_on(async move {
        // Las señales se registran antes de arrancar: un SIGTERM temprano también cierra limpio.
        let mut term = signal(SignalKind::terminate()).map_err(|e| format!("SIGTERM: {e}"))?;
        let mut int = signal(SignalKind::interrupt()).map_err(|e| format!("SIGINT: {e}"))?;
        let senales = async move {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
        };
        let escucha = tokio::net::TcpListener::bind(http).await.map_err(|e| format!("no pude escuchar en {http}: {e}"))?;
        let sesion = Arc::new(ConLimite::new(SesionLibchiaki::new()?, MIN_ENTRE_KEYFRAMES_MS));
        let metricas = Arc::new(Metricas::new());
        let (ordenes, enlace) = ciclo::Enlace::nuevo(metricas.clone());
        // La pausa (consola tomada o reposo) sobrevive a un reinicio: si quedó guardada, se arranca en ella.
        let enlace = enlace.con_pausa(config.pausa.clone());
        // Todo en este runtime (nada de hilos propios con la sesión): un pánico llega al Drop de la sesión.
        // `block_on` corre el ciclo en este hilo; lo que se lanza con `spawn` va a los hilos de trabajo.
        let estado = EstadoServidor::new(Piezas {
            sesion: sesion.clone(),
            clave,
            token_control,
            ordenes,
            metricas,
            sistema: Arc::new(SistemaReal),
            reloj_s: servidor::reloj_sistema_s,
            sesiones_whep: Some(config.sesiones_whep.clone()),
            vigia_estado: config.vigia_estado.clone(),
        });
        tokio::spawn(servidor::muestrear_cpu(estado.clone()));
        // Sin Nagle: los mandos salen enseguida.
        tokio::spawn(servidor::servir(escucha.tap_io(|tcp| { let _ = tcp.set_nodelay(true); }), estado));
        info!(
            "miturno-rp {}: salida MPEG-TS a udp://{dir}, cartel de {} cuadros, {} kbps, servidor en http://{http}",
            env!("CARGO_PKG_VERSION"),
            cartel.video.len(),
            config.bitrate_kbps
        );
        let salida = Salida::new(cartel, destino, salida::origen_del_reloj()).con_procedencia(procedencia);
        ciclo::correr(sesion, perfil, salida, senales, enlace).await?;
        info!("miturno-rp: listo");
        Ok(())
    })
}

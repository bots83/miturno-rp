//! Salida de cuadros del vigilante de imagen en `miturno-kick-out` (config/miturno-kick-out.service).
//!
//! El ffmpeg de kick-out reparte el video decodificado con `split`: una rama va a Kick (sin cambios
//! respecto de main) y la otra saca 1 cuadro de cada 60, a 960×540, a un PPM en tmpfs que lee el
//! clasificador. Lo más delicado: la salida a Kick no puede cambiar ni cortarse, aunque falle la de
//! cuadros.
//!
//! Estos tests usan el `ffmpeg`/`ffprobe` del servidor de video (donde corre `cargo test`), solo con
//! `-f lavfi` y archivos propios en `CARGO_TARGET_TMPDIR`. Ese servidor puede estar transmitiendo a
//! Kick: cada ffmpeg corre con `nice -n 19`, un hilo y clips de 4 s, y de a uno por vez.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// La unidad tal como se instala.
const UNIDAD: &str = include_str!("../../../config/miturno-kick-out.service");

/// Grafo de la unidad: la rama de Kick idéntica a main (salvo `split=2[k][v]`) y la rama del vigía.
const GRAFO: &str = "[0:v:0]split=2[k][v];[k]scale=1280:720,fps=60,format=yuv420p[kick];[v]framestep=60,scale=960:540,format=rgb24[vigia]";

/// Salida de cuadros, después de la de Kick. El esclavo `null` nunca falla: con un solo esclavo,
/// ffmpeg 6.1.1 ignora `onfail=ignore` ("All tee outputs failed.") y termina con 251.
const SALIDA_VIGIA: &str = r#"-map "[vigia]" -an -fps_mode passthrough -c:v ppm -f tee "[f=image2:update=1:atomic_writing=1:onfail=ignore]/run/miturno-vigia/cuadro.ppm|[f=null]-""#;
const CUADRO_UNIDAD: &str = "/run/miturno-vigia/cuadro.ppm";

/// Grafo de main (antes del vigía).
const GRAFO_MAIN: &str = "[0:v:0]split=1[k];[k]scale=1280:720,fps=60,format=yuv420p[kick]";

/// ExecStart de main, byte por byte.
const EXEC_MAIN: &str = r#"ExecStart=/bin/bash -c '\
  set -u; \
  . /usr/local/lib/miturno/kick-destino.sh; \
  if ! destino=$$(kick_destino); then \
    echo "kick-out: falta /etc/miturno-video/kick.env con KICK_RTMP_URL (rtmps://…) y KICK_STREAM_KEY (cargalos con: sudo miturno-configurar-kick)" >&2; \
    exit 78; \
  fi; \
  unset KICK_STREAM_KEY KICK_RTMP_URL KICK_DESTINO; \
  exec /usr/bin/ffmpeg -hide_banner -nostdin -nostats -loglevel warning \
    -rtsp_transport tcp -timeout 5000000 -i rtsp://127.0.0.1:8554/ps5 \
    -filter_complex "[0:v:0]split=1[k];[k]scale=1280:720,fps=60,format=yuv420p[kick]" \
    -map "[kick]" -map 0:a:0 \
    -c:v libx264 -preset veryfast -tune zerolatency -profile:v high -bf 0 \
    -b:v 4500k -maxrate 4500k -bufsize 4500k -g 120 -keyint_min 120 -sc_threshold 0 \
    -x264-params nal-hrd=cbr:keyint=120:min-keyint=120:scenecut=0 \
    -c:a aac -b:a 160k -ar 48000 -ac 2 \
    -f flv -flvflags no_duration_filesize "$$destino" \
    2> >(exec /usr/bin/sed -E -u -e "s#rtmps?://[^[:space:]]*#<destino oculto>#g" -e "s#sk_[A-Za-z0-9_-]+#<clave oculta>#g" >&2)'"#;

const ANCHO: usize = 960;
const ALTO: usize = 540;
const ENCABEZADO_PPM: &[u8] = b"P6\n960 540\n255\n";
const TAM_PPM: usize = 15 + ANCHO * ALTO * 3;

/// 4 s de 1080p60 que se mueve, con audio (la unidad mapea `0:a:0`).
const FUENTE: &str = "testsrc2=size=1920x1080:rate=60[out0];sine=frequency=440:sample_rate=48000[out1]";
const SEGUNDOS: &str = "4";
const CUADROS: u64 = 240;

/// Un ffmpeg a la vez: el servidor de video tiene 2 CPU y puede estar en vivo.
static UNO_POR_VEZ: Mutex<()> = Mutex::new(());

/// Carpeta temporal del test, que se borra al terminar (también si el test falla).
struct Temporal(PathBuf);

impl Temporal {
    fn nueva(nombre: &str) -> Self {
        let ruta = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("kick_out_vigia-{nombre}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&ruta);
        fs::create_dir_all(&ruta).expect("crear la carpeta temporal");
        Temporal(ruta)
    }
}

impl Drop for Temporal {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Mata a ffmpeg si el test falla con el proceso vivo.
struct Proceso(Option<Child>);

impl Drop for Proceso {
    fn drop(&mut self) {
        if let Some(mut hijo) = self.0.take() {
            let _ = hijo.kill();
            let _ = hijo.wait();
        }
    }
}

/// Palabras de un tramo de la unidad tal como las ve ffmpeg (los argumentos de la unidad no llevan
/// espacios adentro de las comillas).
fn palabras(texto: &str) -> Vec<String> {
    texto
        .replace("\\\n", " ")
        .split_whitespace()
        .map(|p| p.trim_matches('"').to_string())
        .collect()
}

/// Argumentos de la salida a Kick de main, desde `-map "[kick]"` hasta antes del destino.
fn args_kick() -> Vec<String> {
    let desde = EXEC_MAIN.find(r#"-map "[kick]""#).expect("salida a Kick en main");
    let hasta = EXEC_MAIN.find(r#""$$destino""#).expect("destino de Kick en main");
    palabras(&EXEC_MAIN[desde..hasta])
}

/// Argumentos de la salida de cuadros con el PPM en `cuadro` en vez de /run/miturno-vigia.
fn args_vigia(cuadro: &Path) -> Vec<String> {
    let cuadro = cuadro.to_str().expect("ruta UTF-8");
    palabras(&SALIDA_VIGIA.replace(CUADRO_UNIDAD, cuadro))
}

fn hay(programa: &str) {
    let ok = Command::new(programa)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(
        ok,
        "falta `{programa}` en el PATH: estos tests corren en el servidor de video (rp.sh test), que siempre lo tiene"
    );
}

/// `nice -n 19 ffmpeg` con un hilo, la fuente de prueba y el grafo de la unidad.
fn ffmpeg(a_ritmo_real: bool) -> Command {
    hay("ffmpeg");
    let mut c = Command::new("nice");
    c.args(["-n", "19", "ffmpeg", "-hide_banner", "-nostdin", "-nostats", "-loglevel", "warning"]);
    c.args(["-filter_complex_threads", "1"]);
    if a_ritmo_real {
        c.args(["-readrate", "1"]);
    }
    c.args(["-f", "lavfi", "-t", SEGUNDOS, "-i", FUENTE]);
    c.args(["-filter_complex", GRAFO]);
    c.stdin(Stdio::null());
    c
}

/// La salida a Kick de la unidad (x264 con un hilo) hacia un `.flv`.
fn salida_kick(c: &mut Command, flv: &Path) {
    c.args(args_kick());
    c.args(["-threads", "1"]);
    c.arg(flv);
}

fn cuadros_flv(flv: &Path) -> u64 {
    hay("ffprobe");
    let salida = Command::new("nice")
        .args(["-n", "19", "ffprobe", "-v", "error", "-threads", "1", "-count_frames"])
        .args(["-select_streams", "v:0", "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
        .arg(flv)
        .output()
        .expect("correr ffprobe");
    assert!(salida.status.success(), "ffprobe falló: {}", String::from_utf8_lossy(&salida.stderr));
    let texto = String::from_utf8_lossy(&salida.stdout);
    texto.trim().parse().unwrap_or_else(|_| panic!("ffprobe devolvió {texto:?}"))
}

fn verificar_ppm(datos: &[u8]) {
    assert_eq!(datos.len(), TAM_PPM, "el PPM tiene que medir 15 + 960·540·3 bytes");
    assert_eq!(&datos[..ENCABEZADO_PPM.len()], ENCABEZADO_PPM, "P6 960×540 de 8 bits");
}

fn exito(salida: &Output) {
    assert!(
        salida.status.success(),
        "ffmpeg salió con {:?}:\n{}",
        salida.status,
        String::from_utf8_lossy(&salida.stderr)
    );
}

fn bloque_exec(unidad: &str) -> &str {
    let desde = unidad.find("\nExecStart=").expect("ExecStart en la unidad") + 1;
    let fin = ">&2)'";
    let hasta = unidad[desde..].find(fin).expect("fin del ExecStart") + desde + fin.len();
    &unidad[desde..hasta]
}

#[test]
fn la_unidad_tiene_el_grafo_la_salida_de_cuadros_y_kick_sin_cambios() {
    let exec = bloque_exec(UNIDAD);
    assert!(
        exec.contains(&format!("    -filter_complex \"{GRAFO}\" \\\n")),
        "la unidad tiene que tener el grafo literal:\n{exec}"
    );
    let linea_vigia = format!("    {SALIDA_VIGIA} \\\n");
    let destino_kick = "\"$$destino\" \\\n";
    assert!(
        exec.contains(&format!("{destino_kick}{linea_vigia}")),
        "la salida de cuadros va literal y justo después de la de Kick:\n{exec}"
    );

    // La rama de Kick del grafo es la de main con split=2.
    assert_eq!(
        GRAFO,
        format!(
            "{};[v]framestep=60,scale=960:540,format=rgb24[vigia]",
            GRAFO_MAIN.replace("split=1[k]", "split=2[k][v]")
        )
    );

    // Sin el grafo nuevo ni la salida de cuadros, el ExecStart es el de main byte por byte.
    let como_main = exec.replace(GRAFO, GRAFO_MAIN).replacen(&linea_vigia, "", 1);
    assert_eq!(como_main, EXEC_MAIN, "la salida a Kick no puede cambiar respecto de main");

    // systemd no tiene nada que interpretar en lo nuevo: sin especificadores (%), variables ($) ni
    // escapes (\), y sin comillas simples que cierren el bash -c.
    for nuevo in [GRAFO, SALIDA_VIGIA] {
        for c in ['%', '$', '\\', '\''] {
            assert!(!nuevo.contains(c), "{c:?} en {nuevo}");
        }
    }

    let lineas: Vec<&str> = UNIDAD.lines().collect();
    for propiedad in ["RuntimeDirectory=miturno-vigia", "RuntimeDirectoryMode=0755"] {
        assert!(lineas.contains(&propiedad), "falta {propiedad} en la unidad");
    }
    assert!(!UNIDAD.contains("Reserva para F2"), "el comentario de la reserva se reemplaza");
}

#[test]
fn kick_sale_entera_y_el_cuadro_es_un_ppm_completo() {
    let _turno = UNO_POR_VEZ.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = Temporal::nueva("completo");
    let flv = tmp.0.join("kick.flv");
    let carpeta = tmp.0.join("vigia");
    fs::create_dir(&carpeta).unwrap();
    let cuadro = carpeta.join("cuadro.ppm");

    let mut c = ffmpeg(false);
    salida_kick(&mut c, &flv);
    c.args(args_vigia(&cuadro));
    let hijo = c.stdout(Stdio::null()).stderr(Stdio::piped()).spawn().expect("lanzar ffmpeg");
    let mut proceso = Proceso(Some(hijo));

    // Mientras escribe, el cuadro que se lee siempre está entero (nunca el .tmp a medias).
    let mut lecturas = 0u32;
    while proceso.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        if let Ok(datos) = fs::read(&cuadro) {
            verificar_ppm(&datos);
            lecturas += 1;
        }
        sleep(Duration::from_millis(5));
    }
    let salida = proceso.0.take().unwrap().wait_with_output().unwrap();
    exito(&salida);

    assert_eq!(cuadros_flv(&flv), CUADROS, "4 s a 60 fps por Kick");
    verificar_ppm(&fs::read(&cuadro).expect("el cuadro final existe"));
    let mut quedan: Vec<String> = fs::read_dir(&carpeta)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    quedan.sort();
    assert_eq!(quedan, ["cuadro.ppm"], "sin temporales a medias");
    eprintln!("lecturas del cuadro mientras escribía: {lecturas}");
}

#[test]
fn la_rama_vigia_saca_un_cuadro_de_cada_60_sin_duplicar() {
    let _turno = UNO_POR_VEZ.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = Temporal::nueva("cuenta");
    let carpeta = tmp.0.join("vigia");
    fs::create_dir(&carpeta).unwrap();

    // Mismo grafo y misma salida, pero cada cuadro a su archivo (sin update) para contarlos. Kick
    // va a null: acá no se mide.
    let mut c = ffmpeg(false);
    c.args(["-map", "[kick]", "-map", "0:a:0", "-f", "null", "-"]);
    let numerados = carpeta.join("%03d.ppm");
    c.args(palabras(
        &SALIDA_VIGIA
            .replace(CUADRO_UNIDAD, numerados.to_str().unwrap())
            .replace("update=1:atomic_writing=1:", ""),
    ));
    let salida = c.stdout(Stdio::null()).stderr(Stdio::piped()).output().expect("correr ffmpeg");
    exito(&salida);

    let mut archivos: Vec<String> = fs::read_dir(&carpeta)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    archivos.sort();
    assert_eq!(archivos, ["001.ppm", "002.ppm", "003.ppm", "004.ppm"], "240 cuadros / 60");
    for a in &archivos {
        verificar_ppm(&fs::read(carpeta.join(a)).unwrap());
    }
}

/// Corre la unidad a ritmo real, borra la carpeta de cuadros a los 1,5 s y, si `parar_a` viene, manda
/// SIGTERM a ffmpeg en ese momento (como `kick_off`). Devuelve la salida de ffmpeg y los cuadros del `.flv`.
fn kick_con_la_salida_de_cuadros_rota(nombre: &str, parar_a: Option<Duration>) -> (Output, u64) {
    let tmp = Temporal::nueva(nombre);
    let flv = tmp.0.join("kick.flv");
    let carpeta = tmp.0.join("vigia");
    fs::create_dir(&carpeta).unwrap();
    let cuadro = carpeta.join("cuadro.ppm");

    let mut c = ffmpeg(true);
    salida_kick(&mut c, &flv);
    c.args(args_vigia(&cuadro));
    let inicio = Instant::now();
    let hijo = c.stdout(Stdio::null()).stderr(Stdio::piped()).spawn().expect("lanzar ffmpeg");
    let mut proceso = Proceso(Some(hijo));

    // A ritmo real salen cuadros del vigía en t = 0, 1, 2 y 3 s. Se borra la carpeta a los 1,5 s
    // (después de ver el primer cuadro): fallan los de 2 y 3 s.
    while !cuadro.exists() {
        assert!(inicio.elapsed() < Duration::from_secs(3), "no apareció el primer cuadro");
        assert!(proceso.0.as_mut().unwrap().try_wait().unwrap().is_none(), "ffmpeg terminó antes de tiempo");
        sleep(Duration::from_millis(10));
    }
    sleep(Duration::from_millis(1500).saturating_sub(inicio.elapsed()));
    fs::remove_dir_all(&carpeta).expect("borrar la carpeta de cuadros");

    if let Some(momento) = parar_a {
        sleep(momento.saturating_sub(inicio.elapsed()));
        let pid = proceso.0.as_ref().unwrap().id().to_string();
        // `nice` hace exec de ffmpeg: el pid es el de ffmpeg.
        let ok = Command::new("kill").args(["-TERM", &pid]).status().expect("correr kill").success();
        assert!(ok, "no se pudo mandar SIGTERM a ffmpeg");
    }

    let salida = proceso.0.take().unwrap().wait_with_output().unwrap();
    let errores = String::from_utf8_lossy(&salida.stderr);
    eprintln!("stderr de ffmpeg:\n{errores}");
    assert!(errores.contains("cuadro.ppm"), "la salida de cuadros tuvo que fallar:\n{errores}");
    assert!(!carpeta.exists(), "la salida de cuadros no recrea la carpeta");
    let cuadros = cuadros_flv(&flv);
    (salida, cuadros)
}

/// Review Focus 1: la salida de cuadros falla a mitad (carpeta borrada) y Kick sigue entera.
#[test]
fn si_la_salida_de_cuadros_falla_kick_sigue_entera() {
    let _turno = UNO_POR_VEZ.lock().unwrap_or_else(|e| e.into_inner());
    let (salida, cuadros) = kick_con_la_salida_de_cuadros_rota("falla", None);
    exito(&salida);
    assert_eq!(cuadros, CUADROS, "Kick sin cortes aunque falle la salida de cuadros");
}

/// Con la salida de cuadros rota, `kick_off` (SIGTERM) sigue saliendo con 255, como hoy
/// (`SuccessExitStatus=255`): la unidad no queda `failed`.
#[test]
fn con_la_salida_de_cuadros_rota_sigterm_sale_con_255() {
    let _turno = UNO_POR_VEZ.lock().unwrap_or_else(|e| e.into_inner());
    let (salida, cuadros) = kick_con_la_salida_de_cuadros_rota("sigterm", Some(Duration::from_millis(2500)));
    assert_eq!(
        salida.status.code(),
        Some(255),
        "ffmpeg ante SIGTERM:\n{}",
        String::from_utf8_lossy(&salida.stderr)
    );
    assert!(cuadros > 0 && cuadros < CUADROS, "Kick hasta el SIGTERM: {cuadros} cuadros");
}

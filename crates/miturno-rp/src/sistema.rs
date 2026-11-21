//! Lo que el servidor consulta o cambia de la máquina: estado de unidades de systemd, arrancar o
//! parar la salida a Kick y los tiempos de CPU de `/proc/stat`. Detrás de un trait para que los tests
//! no dependan del host.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub type Futuro<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Tiempos acumulados de CPU de toda la máquina (en jiffies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TiemposCpu {
    pub total: u64,
    /// Ociosa más esperando disco.
    pub ociosa: u64,
}

pub trait Sistema: Send + Sync + 'static {
    /// `systemctl is-active` de cada unidad, en el mismo orden (`"desconocido"` si no se pudo saber).
    fn estados<'a>(&'a self, unidades: &'a [&'a str]) -> Futuro<'a, Vec<String>>;
    /// `systemctl <verbo> <unidad>` (sin esperar al trabajo). El error es para el registro.
    fn systemctl<'a>(&'a self, verbo: &'a str, unidad: &'a str) -> Futuro<'a, Result<(), String>>;
    fn tiempos_cpu(&self) -> Option<TiemposCpu>;
}

/// Plazo para cualquier llamada a `systemctl`.
const PLAZO_SYSTEMCTL: Duration = Duration::from_secs(3);

/// La máquina de verdad.
pub struct SistemaReal;

impl Sistema for SistemaReal {
    fn estados<'a>(&'a self, unidades: &'a [&'a str]) -> Futuro<'a, Vec<String>> {
        Box::pin(async move {
            let salida = tokio::time::timeout(
                PLAZO_SYSTEMCTL,
                tokio::process::Command::new("systemctl").arg("is-active").args(unidades).kill_on_drop(true).output(),
            )
            .await;
            // `is-active` sale con error si alguna no está activa, pero igual imprime una línea por unidad.
            let texto = match salida {
                Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).into_owned(),
                _ => String::new(),
            };
            completar_estados(&texto, unidades.len())
        })
    }

    fn systemctl<'a>(&'a self, verbo: &'a str, unidad: &'a str) -> Futuro<'a, Result<(), String>> {
        Box::pin(async move {
            let salida = tokio::time::timeout(
                PLAZO_SYSTEMCTL,
                tokio::process::Command::new("systemctl").args(["--no-block", verbo, unidad]).kill_on_drop(true).output(),
            )
            .await;
            match salida {
                Err(_) => Err(format!("systemctl {verbo} {unidad}: no terminó en {} s", PLAZO_SYSTEMCTL.as_secs())),
                Ok(Err(e)) => Err(format!("systemctl {verbo} {unidad}: {e}")),
                Ok(Ok(o)) if o.status.success() => Ok(()),
                Ok(Ok(o)) => Err(format!(
                    "systemctl {verbo} {unidad}: {} ({})",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                )),
            }
        })
    }

    fn tiempos_cpu(&self) -> Option<TiemposCpu> {
        parsear_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
    }
}

/// Una línea por unidad; lo que falte es `"desconocido"`.
pub fn completar_estados(texto: &str, n: usize) -> Vec<String> {
    let mut v: Vec<String> = texto.lines().map(|l| l.trim().to_string()).take(n).collect();
    v.resize(n, "desconocido".to_string());
    v
}

/// La línea `cpu` de `/proc/stat`: user nice system idle iowait irq softirq steal (guest ya va en user).
pub fn parsear_proc_stat(texto: &str) -> Option<TiemposCpu> {
    let linea = texto.lines().find(|l| l.split_whitespace().next() == Some("cpu"))?;
    let campos: Vec<u64> = linea.split_whitespace().skip(1).take(8).map(str::parse).collect::<Result<_, _>>().ok()?;
    if campos.len() < 4 {
        return None;
    }
    let total = campos.iter().sum();
    let ociosa = campos[3] + campos.get(4).copied().unwrap_or(0);
    Some(TiemposCpu { total, ociosa })
}

/// Porcentaje de CPU ocupada entre dos muestras (0 a 100, con un decimal). Sin avance, 0.
pub fn cpu_por_ciento(antes: TiemposCpu, ahora: TiemposCpu) -> f64 {
    let total = ahora.total.saturating_sub(antes.total);
    if total == 0 || ahora.total < antes.total || ahora.ociosa < antes.ociosa {
        return 0.0;
    }
    let ociosa = (ahora.ociosa - antes.ociosa).min(total);
    let p = (total - ociosa) as f64 * 100.0 / total as f64;
    (p * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "cpu  100 5 50 800 20 3 2 10 7 0\ncpu0 50 2 25 400 10 1 1 5 3 0\nintr 12345\n";

    #[test]
    fn lee_la_linea_cpu_de_proc_stat() {
        // total = 100+5+50+800+20+3+2+10 = 990; ociosa = 800+20 = 820 (guest no se suma: ya está en user).
        assert_eq!(parsear_proc_stat(STAT), Some(TiemposCpu { total: 990, ociosa: 820 }));
        assert_eq!(parsear_proc_stat("cpu0 1 2 3 4\n"), None, "sin la línea total");
        assert_eq!(parsear_proc_stat("cpu  1 2 x 4 5 6 7 8\n"), None);
        assert_eq!(parsear_proc_stat(""), None);
        // Núcleos viejos con solo 4 campos.
        assert_eq!(parsear_proc_stat("cpu  10 0 10 80\n"), Some(TiemposCpu { total: 100, ociosa: 80 }));
    }

    #[test]
    fn porcentaje_entre_dos_muestras() {
        let a = TiemposCpu { total: 1_000, ociosa: 800 };
        assert_eq!(cpu_por_ciento(a, TiemposCpu { total: 2_000, ociosa: 1_550 }), 25.0);
        assert_eq!(cpu_por_ciento(a, TiemposCpu { total: 1_300, ociosa: 800 }), 100.0);
        assert_eq!(cpu_por_ciento(a, a), 0.0, "sin avance");
        assert_eq!(cpu_por_ciento(a, TiemposCpu { total: 900, ociosa: 700 }), 0.0, "contadores reiniciados");
        assert_eq!(cpu_por_ciento(a, TiemposCpu { total: 1_003, ociosa: 801 }), 66.7);
    }

    #[test]
    fn estados_completa_lo_que_falta() {
        assert_eq!(completar_estados("active\ninactive\n", 3), vec!["active", "inactive", "desconocido"]);
        assert_eq!(completar_estados("", 2), vec!["desconocido", "desconocido"]);
        assert_eq!(completar_estados("active\nfailed\nactivating\nextra\n", 3), vec!["active", "failed", "activating"]);
    }
}

//! Filtro por rol: lo único que separa a un jugador de los menús del sistema de la PS5.
//! Un jugador nunca puede apretar PS ni Create (abren menú, tienda, mensajes, perfil: riesgo de ban).

use crate::credencial::Rol;
use crate::mando::{EstadoMando, CREATE, OPTIONS, PS};

/// Límites del botón Options para el rol `Jugador`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitesOptions {
    /// Tiempo mínimo entre el inicio de un toque aceptado y el siguiente.
    pub intervalo_ms: u64,
    /// Máximo que se deja Options apretado antes de forzarlo a suelto.
    pub max_apretado_ms: u64,
}

impl Default for LimitesOptions {
    fn default() -> Self {
        Self { intervalo_ms: 30_000, max_apretado_ms: 2_000 }
    }
}

const PROHIBIDOS: u32 = PS | CREATE;

#[derive(Debug, Clone)]
pub struct Filtro {
    rol: Rol,
    limites: LimitesOptions,
    descartados: u64,
    /// Bits PS/Create de la entrada anterior (para contar flancos de subida).
    prohibidos_previos: u32,
    options_previo: bool,
    /// Inicio del último toque de Options aceptado.
    ultimo_inicio: Option<u64>,
    /// El toque actual de Options está aceptado y en curso.
    options_desde: Option<u64>,
    /// Options queda suelto hasta que la entrada lo suelte de verdad.
    options_bloqueado: bool,
}

impl Filtro {
    pub fn new(rol: Rol, limites: LimitesOptions) -> Self {
        Self {
            rol,
            limites,
            descartados: 0,
            prohibidos_previos: 0,
            options_previo: false,
            ultimo_inicio: None,
            options_desde: None,
            options_bloqueado: false,
        }
    }

    /// Devuelve lo que realmente puede llegar a la PS5 según el rol.
    pub fn aplicar(&mut self, entrada: &EstadoMando, ahora_ms: u64) -> EstadoMando {
        match self.rol {
            Rol::Operador => *entrada,
            Rol::Vista => EstadoMando::suelto(),
            Rol::Jugador => self.aplicar_jugador(entrada, ahora_ms),
        }
    }

    /// Apretadas descartadas (una por apretada, no por mensaje).
    pub fn descartados(&self) -> u64 {
        self.descartados
    }

    fn aplicar_jugador(&mut self, entrada: &EstadoMando, ahora_ms: u64) -> EstadoMando {
        let mut salida = *entrada;

        // PS y Create: nunca pasan; cada apretada nueva cuenta una vez.
        let prohibidos = entrada.botones & PROHIBIDOS;
        self.descartados += u64::from((prohibidos & !self.prohibidos_previos).count_ones());
        self.prohibidos_previos = prohibidos;
        salida.botones &= !PROHIBIDOS;

        // Options: limitado en frecuencia y en duración.
        let abajo = entrada.botones & OPTIONS != 0;
        if !abajo {
            self.options_bloqueado = false;
            self.options_desde = None;
        } else {
            if !self.options_previo {
                let permitido = match self.ultimo_inicio {
                    None => true,
                    Some(t) => ahora_ms.saturating_sub(t) >= self.limites.intervalo_ms,
                };
                if permitido {
                    self.ultimo_inicio = Some(ahora_ms);
                    self.options_desde = Some(ahora_ms);
                } else {
                    self.descartados += 1;
                    self.options_bloqueado = true;
                }
            }
            if !self.options_bloqueado {
                if let Some(desde) = self.options_desde {
                    if ahora_ms.saturating_sub(desde) > self.limites.max_apretado_ms {
                        self.options_bloqueado = true;
                    }
                }
            }
        }
        self.options_previo = abajo;
        if !abajo || self.options_bloqueado {
            salida.botones &= !OPTIONS;
        }
        salida
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mando::*;

    fn con(botones: u32) -> EstadoMando {
        EstadoMando { botones, l2: 200, r2: 100, lx: -32768, ly: 32767, rx: 5, ry: -5 }
    }

    fn jugador() -> Filtro {
        Filtro::new(Rol::Jugador, LimitesOptions::default())
    }

    #[test]
    fn limites_por_defecto() {
        let l = LimitesOptions::default();
        assert_eq!((l.intervalo_ms, l.max_apretado_ms), (30_000, 2_000));
    }

    #[test]
    fn operador_deja_pasar_ps_create_y_options() {
        let mut f = Filtro::new(Rol::Operador, LimitesOptions::default());
        let e = con(PS | CREATE | OPTIONS | CRUZ);
        assert_eq!(f.aplicar(&e, 0), e);
        assert_eq!(f.aplicar(&e, 10_000), e);
        assert_eq!(f.descartados(), 0);
    }

    #[test]
    fn vista_devuelve_siempre_suelto() {
        let mut f = Filtro::new(Rol::Vista, LimitesOptions::default());
        assert_eq!(f.aplicar(&con(CRUZ | PS | OPTIONS), 0), EstadoMando::suelto());
        assert_eq!(f.descartados(), 0);
    }

    #[test]
    fn jugador_nunca_pasa_ps_ni_create_aunque_los_mantenga() {
        let mut f = jugador();
        let e = con(PS | CREATE | CRUZ);
        for t in [0, 1, 500, 5_000, 10_000] {
            let s = f.aplicar(&e, t);
            assert_eq!(s.botones & (PS | CREATE), 0);
            assert_eq!(s.botones & CRUZ, CRUZ);
        }
    }

    #[test]
    fn ps_y_create_suman_un_descarte_por_apretada() {
        let mut f = jugador();
        f.aplicar(&con(PS), 0);
        f.aplicar(&con(PS), 10_000);
        assert_eq!(f.descartados(), 1);
        f.aplicar(&con(0), 10_001);
        f.aplicar(&con(CREATE), 10_002);
        assert_eq!(f.descartados(), 2);
        f.aplicar(&con(PS | CREATE), 10_003);
        assert_eq!(f.descartados(), 3);
    }

    #[test]
    fn jugador_pasa_el_resto_de_botones_gatillos_y_sticks() {
        let mut f = jugador();
        let resto = CRUZ | CIRCULO | CUADRADO | TRIANGULO | L1 | R1 | L3 | R3 | PANEL | ARRIBA | ABAJO | IZQUIERDA | DERECHA;
        let e = con(resto);
        assert_eq!(f.aplicar(&e, 0), e);
        assert_eq!(f.descartados(), 0);
    }

    fn toque(f: &mut Filtro, t: u64) -> u32 {
        let s = f.aplicar(&con(OPTIONS), t).botones & OPTIONS;
        f.aplicar(&con(0), t + 50);
        s
    }

    #[test]
    fn options_primer_toque_pasa() {
        let mut f = jugador();
        assert_eq!(f.aplicar(&con(OPTIONS), 0).botones, OPTIONS);
        assert_eq!(f.descartados(), 0);
    }

    #[test]
    fn options_a_los_29_s_se_descarta_y_a_los_30_s_pasa() {
        let mut f = jugador();
        assert_eq!(toque(&mut f, 0), OPTIONS);
        assert_eq!(toque(&mut f, 29_000), 0);
        assert_eq!(f.descartados(), 1);
        assert_eq!(toque(&mut f, 30_000), OPTIONS);
        assert_eq!(f.descartados(), 1);
    }

    #[test]
    fn options_la_ventana_se_mide_desde_el_inicio_del_toque_aceptado() {
        let mut f = jugador();
        // Toque aceptado que arranca en 0 y dura 1 s.
        assert_eq!(f.aplicar(&con(OPTIONS), 0).botones, OPTIONS);
        assert_eq!(f.aplicar(&con(OPTIONS), 1_000).botones, OPTIONS);
        f.aplicar(&con(0), 1_001);
        // 29,5 s después del inicio (28,5 s del final): descartado.
        assert_eq!(toque(&mut f, 29_500), 0);
        // Desde el inicio ya pasaron 30 s: pasa.
        assert_eq!(toque(&mut f, 30_000), OPTIONS);
    }

    #[test]
    fn options_mantenido_se_fuerza_suelto_a_los_2001_ms_hasta_que_se_suelte() {
        let mut f = jugador();
        assert_eq!(f.aplicar(&con(OPTIONS), 0).botones, OPTIONS);
        assert_eq!(f.aplicar(&con(OPTIONS), 2_000).botones, OPTIONS);
        assert_eq!(f.aplicar(&con(OPTIONS), 2_001).botones, 0);
        assert_eq!(f.aplicar(&con(OPTIONS), 40_000).botones, 0);
        assert_eq!(f.descartados(), 0);
        // Soltar de verdad y volver a apretar fuera de la ventana: pasa de nuevo.
        f.aplicar(&con(0), 40_001);
        assert_eq!(f.aplicar(&con(OPTIONS), 40_002).botones, OPTIONS);
    }

    #[test]
    fn options_toque_nuevo_dentro_de_30_s_del_inicio_sigue_descartado() {
        let mut f = jugador();
        f.aplicar(&con(OPTIONS), 0);
        f.aplicar(&con(OPTIONS), 2_001); // forzado a suelto
        f.aplicar(&con(0), 2_002);
        assert_eq!(f.aplicar(&con(OPTIONS), 2_500).botones, 0);
        assert_eq!(f.descartados(), 1);
        // Mantenerlo apretado no suma más descartes.
        assert_eq!(f.aplicar(&con(OPTIONS), 9_000).botones, 0);
        assert_eq!(f.descartados(), 1);
    }

    #[test]
    fn options_descartado_no_se_cuela_aunque_se_mantenga_hasta_pasar_la_ventana() {
        let mut f = jugador();
        toque(&mut f, 0);
        assert_eq!(f.aplicar(&con(OPTIONS), 10_000).botones, 0);
        assert_eq!(f.aplicar(&con(OPTIONS), 31_000).botones, 0);
        assert_eq!(f.descartados(), 1);
    }
}

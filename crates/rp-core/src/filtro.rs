//! Filtro por rol: lo único que separa a un jugador de los menús del sistema de la PS5.
//! `ahora_ms` debe salir de un reloj monótono (nunca de la hora de pared). Cada mensaje válido del
//! cliente debe llamar también a `Guardian::latido`.
//! Un jugador nunca puede apretar PS ni Create (abren menú, tienda, mensajes, perfil: riesgo de ban).

use crate::credencial::Rol;
use crate::mando::{EstadoMando, CONOCIDOS, CREATE, PS};

const PROHIBIDOS: u32 = PS | CREATE;

#[derive(Debug, Clone)]
pub struct Filtro {
    rol: Rol,
    descartados: u64,
    /// Bits PS/Create de la entrada anterior (para contar flancos de subida).
    prohibidos_previos: u32,
}

impl Filtro {
    pub fn new(rol: Rol) -> Self {
        Self { rol, descartados: 0, prohibidos_previos: 0 }
    }

    /// Devuelve lo que realmente puede llegar a la PS5 según el rol. Los bits desconocidos se
    /// descartan siempre.
    pub fn aplicar(&mut self, entrada: &EstadoMando, _ahora_ms: u64) -> EstadoMando {
        match self.rol {
            Rol::Operador => EstadoMando { botones: entrada.botones & CONOCIDOS, ..*entrada },
            Rol::Vista => EstadoMando::suelto(),
            Rol::Jugador => self.aplicar_jugador(entrada),
        }
    }

    /// Apretadas descartadas (una por apretada, no por mensaje). PS y Create apretados juntos
    /// cuentan 2.
    pub fn descartados(&self) -> u64 {
        self.descartados
    }

    fn aplicar_jugador(&mut self, entrada: &EstadoMando) -> EstadoMando {
        // PS y Create: nunca pasan; cada apretada nueva cuenta una vez.
        let prohibidos = entrada.botones & PROHIBIDOS;
        self.descartados += u64::from((prohibidos & !self.prohibidos_previos).count_ones());
        self.prohibidos_previos = prohibidos;
        // Options es libre para el jugador (sin ventana ni máximo apretado): salir del juego lo
        // frena el watchdog de imagen (F2) y la cuenta PSN dedicada con control parental.
        EstadoMando { botones: entrada.botones & CONOCIDOS & !PROHIBIDOS, ..*entrada }
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
        Filtro::new(Rol::Jugador)
    }

    #[test]
    fn operador_deja_pasar_ps_create_y_options() {
        let mut f = Filtro::new(Rol::Operador);
        let e = con(PS | CREATE | OPTIONS | CRUZ);
        assert_eq!(f.aplicar(&e, 0), e);
        assert_eq!(f.aplicar(&e, 10_000), e);
        assert_eq!(f.descartados(), 0);
    }

    #[test]
    fn vista_devuelve_siempre_suelto() {
        let mut f = Filtro::new(Rol::Vista);
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
    fn ps_y_create_juntos_cuentan_2_y_no_suman_mientras_siguen_apretados() {
        let mut f = jugador();
        f.aplicar(&con(PS | CREATE), 0);
        assert_eq!(f.descartados(), 2);
        f.aplicar(&con(PS | CREATE), 10_000);
        assert_eq!(f.descartados(), 2);
    }

    #[test]
    fn jugador_pasa_el_resto_de_botones_gatillos_y_sticks() {
        let mut f = jugador();
        let resto = CRUZ | CIRCULO | CUADRADO | TRIANGULO | L1 | R1 | L3 | R3 | PANEL | ARRIBA | ABAJO | IZQUIERDA | DERECHA;
        let e = con(resto);
        assert_eq!(f.aplicar(&e, 0), e);
        assert_eq!(f.descartados(), 0);
    }

    #[test]
    fn jugador_options_dos_veces_en_menos_de_1_s_pasan_las_dos() {
        let mut f = jugador();
        assert_eq!(f.aplicar(&con(OPTIONS), 0).botones, OPTIONS);
        f.aplicar(&con(0), 100);
        assert_eq!(f.aplicar(&con(OPTIONS), 500).botones, OPTIONS);
        assert_eq!(f.descartados(), 0);
    }

    #[test]
    fn jugador_options_mantenido_10_s_sigue_apretado() {
        let mut f = jugador();
        let e = con(OPTIONS | CRUZ);
        for t in [0, 1, 2_000, 2_001, 5_000, 10_000] {
            assert_eq!(f.aplicar(&e, t).botones, OPTIONS | CRUZ);
        }
        assert_eq!(f.descartados(), 0);
    }

    #[test]
    fn jugador_options_no_suma_descartes_aunque_se_apriete_muchas_veces() {
        let mut f = jugador();
        for t in 0..20u64 {
            f.aplicar(&con(OPTIONS), t * 10);
            f.aplicar(&con(0), t * 10 + 5);
        }
        assert_eq!(f.descartados(), 0);
    }

    #[test]
    fn bits_desconocidos_nunca_pasan() {
        let mut f = jugador();
        assert_eq!(f.aplicar(&con((1 << 16) | (1 << 31) | CRUZ), 0).botones, CRUZ);
        let mut o = Filtro::new(Rol::Operador);
        assert_eq!(o.aplicar(&con((1 << 16) | (1 << 31) | PS | CRUZ), 0).botones, PS | CRUZ);
    }
}

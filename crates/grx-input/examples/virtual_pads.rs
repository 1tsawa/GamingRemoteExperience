use grx_input::{xbutton, GamepadSink, GamepadState, Rumble, VigemGamepads, VirtualPad};

#[cfg(windows)]
fn main() {
    // Analyse des arguments positionnels optionnels : [secs] [mode]
    let args: Vec<String> = std::env::args().collect();
    let duration_secs: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(8.0);
    let mode = args.get(2).map(|s| s.as_str()).unwrap_or("both");

    // Création du canal de réception des vibrations (rumble)
    let (tx_rumble, rx_rumble) = std::sync::mpsc::sync_channel::<Rumble>(64);

    // Initialisation du gestionnaire de manettes virtuelles ViGEm
    let mut vigem = VigemGamepads::new();

    // Configuration du canal de vibrations AVANT le premier update
    vigem.set_rumble_sink(tx_rumble);

    // Définition des états statiques des manettes
    let state_index_0 = GamepadState {
        index: 0,
        buttons: xbutton::A,
        left_trigger: 128,
        thumb_lx: 20000,
        thumb_ly: -10000,
        ..Default::default()
    };

    let state_index_1 = GamepadState {
        index: 1,
        buttons: xbutton::A | xbutton::GUIDE | xbutton::TOUCHPAD,
        right_trigger: 200,
        thumb_rx: -20000,
        ..Default::default()
    };

    // Configuration des manettes virtuelles selon le mode choisi
    let mut active_indices: Vec<u8> = Vec::new();
    match mode {
        "x360" => {
            vigem.set_virtual(0, VirtualPad::Xbox360);
            active_indices.push(0);
        }
        "ds4" => {
            vigem.set_virtual(0, VirtualPad::DualShock4);
            active_indices.push(0);
        }
        _ => {
            vigem.set_virtual(0, VirtualPad::Xbox360);
            vigem.set_virtual(1, VirtualPad::DualShock4);
            active_indices.push(0);
            active_indices.push(1);
        }
    }

    // Premier update pour enregistrer les manettes et vérifier le branchement
    for &idx in &active_indices {
        let state = if idx == 0 { &state_index_0 } else { &state_index_1 };
        if let Err(e) = vigem.update(state) {
            eprintln!("Erreur lors de la première mise à jour : {}", e);
            std::process::exit(1);
        }
    }
    println!("Manettes branchées : {}", vigem.plugged_count());

    // Boucle principale : envoi des états toutes les 20 ms jusqu'à expiration du délai
    let interval = std::time::Duration::from_millis(20);
    let duration = std::time::Duration::from_secs_f64(duration_secs);
    let start = std::time::Instant::now();
    let mut next_tick = start + interval;
    let mut total_rumbles = 0u32;

    while start.elapsed() < duration {
        // Envoi des états pour chaque index actif
        for &idx in &active_indices {
            let state = if idx == 0 { &state_index_0 } else { &state_index_1 };
            if let Err(e) = vigem.update(state) {
                eprintln!("Erreur lors de la mise à jour : {}", e);
                std::process::exit(1);
            }
        }

        // Drainage du canal des vibrations et affichage
        while let Ok(rumble) = rx_rumble.try_recv() {
            println!("vibration index {} gros {} petit {}", rumble.index, rumble.large, rumble.small);
            total_rumbles += 1;
        }

        // Synchronisation de la fréquence d'envoi (20 ms)
        let now = std::time::Instant::now();
        if now < next_tick {
            std::thread::sleep(next_tick - now);
        }
        next_tick += interval;
    }

    // Nettoyage : déconnexion des manettes virtuelles
    for &idx in &active_indices {
        vigem.disconnect(idx);
    }

    println!("Total vibrations reçues : {}", total_rumbles);
    println!("manettes virtuelles debranchees");
}

#[cfg(not(windows))]
fn main() {
    println!("Cet exemple est réservé à Windows (nécessite le pilote ViGEm).");
}

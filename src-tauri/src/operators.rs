// Best-effort operator (airline / military command) from a callsign, computed
// offline at ingest — no network. ADS-B flight IDs carry an ICAO telephony
// prefix: three letters then a flight number for airlines ("UAL1381" → United),
// or a tactical/support word for military ("RCH285" → USAF Air Mobility).
//
// Coverage is a curated subset of the busiest carriers plus high-confidence
// military callsigns; anything military-flagged but unmatched falls back to a
// bare "Military". Unknown civil callsigns (bare N-number tail, foreign GA)
// return None rather than guess.

/// Airline ICAO designators → operator name. Matched only when the callsign is
/// exactly three letters followed by a digit (the airline flight-number shape),
/// so a bare tail like "N4872D" never false-matches.
const AIRLINES: &[(&str, &str)] = &[
    // US major / low-cost
    ("UAL", "United Airlines"), ("AAL", "American Airlines"), ("DAL", "Delta Air Lines"),
    ("SWA", "Southwest Airlines"), ("JBU", "JetBlue"), ("ASA", "Alaska Airlines"),
    ("NKS", "Spirit Airlines"), ("FFT", "Frontier Airlines"), ("HAL", "Hawaiian Airlines"),
    ("SCX", "Sun Country Airlines"), ("AAY", "Allegiant Air"),
    // US regional
    ("SKW", "SkyWest Airlines"), ("RPA", "Republic Airways"), ("EDV", "Endeavor Air"),
    ("ENY", "Envoy Air"), ("JIA", "PSA Airlines"), ("AWI", "Air Wisconsin"),
    ("GJS", "GoJet Airlines"), ("QXE", "Horizon Air"), ("ASH", "Mesa Airlines"),
    ("UCA", "CommutAir"),
    // Cargo
    ("FDX", "FedEx Express"), ("UPS", "UPS Airlines"), ("ABX", "ABX Air"),
    ("GTI", "Atlas Air"), ("CKS", "Kalitta Air"), ("ATN", "Air Transport Intl"),
    ("GEC", "Lufthansa Cargo"), ("CLX", "Cargolux"), ("NCA", "Nippon Cargo"),
    // Canada / Latin America
    ("ACA", "Air Canada"), ("ROU", "Air Canada Rouge"), ("WJA", "WestJet"),
    ("JZA", "Jazz Aviation"), ("TSC", "Air Transat"), ("AMX", "Aeroméxico"),
    ("VOI", "Volaris"), ("AVA", "Avianca"), ("CMP", "Copa Airlines"),
    ("LAN", "LATAM Airlines"), ("AZU", "Azul"), ("GLO", "Gol"),
    // Europe
    ("BAW", "British Airways"), ("VIR", "Virgin Atlantic"), ("DLH", "Lufthansa"),
    ("AFR", "Air France"), ("KLM", "KLM"), ("EZY", "easyJet"), ("RYR", "Ryanair"),
    ("IBE", "Iberia"), ("SWR", "Swiss"), ("AUA", "Austrian Airlines"),
    ("SAS", "Scandinavian Airlines"), ("TAP", "TAP Air Portugal"), ("EIN", "Aer Lingus"),
    ("FIN", "Finnair"), ("NAX", "Norwegian"), ("THY", "Turkish Airlines"),
    ("AEE", "Aegean Airlines"),
    // Middle East / Asia / Oceania
    ("UAE", "Emirates"), ("QTR", "Qatar Airways"), ("ETD", "Etihad Airways"),
    ("ELY", "El Al"), ("SVA", "Saudia"), ("ANA", "All Nippon Airways"),
    ("JAL", "Japan Airlines"), ("KAL", "Korean Air"), ("AAR", "Asiana Airlines"),
    ("CPA", "Cathay Pacific"), ("SIA", "Singapore Airlines"), ("CCA", "Air China"),
    ("CES", "China Eastern"), ("CSN", "China Southern"), ("QFA", "Qantas"),
    ("ANZ", "Air New Zealand"),
    // Business / fractional
    ("EJA", "NetJets"), ("LXJ", "Flexjet"), ("XOJ", "XOJET"),
];

/// Military callsign prefixes → command. Matched on the callsign's leading
/// letter run (any length), before the airline check.
const MIL: &[(&str, &str)] = &[
    ("RCH", "USAF · Air Mobility"),   // Reach
    ("SPAR", "USAF · Special Air Mission"),
    ("SAM", "USAF · Special Air Mission"),
    ("PAT", "US Army · Priority Air Transport"),
    ("CNV", "US Navy · Convoy"),
    ("VVBG", "US Navy"),
    ("EVAC", "US Military · Aeromedical"),
    ("GRZLY", "US Military"),
];

/// Operator for a callsign, or None. `military` is the ADS-B military db-flag,
/// used only as the fallback when no callsign prefix matches.
pub fn operator_for(callsign: Option<&str>, military: bool) -> Option<String> {
    let cs = callsign.map(str::trim).filter(|s| !s.is_empty());
    if let Some(cs) = cs {
        let up = cs.to_ascii_uppercase();
        let alpha: String = up.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        if alpha.len() >= 3 {
            if let Some((_, name)) = MIL.iter().find(|(p, _)| *p == alpha) {
                return Some((*name).to_string());
            }
            // airline shape: exactly 3 letters immediately followed by a digit
            let rest = &up[alpha.len()..];
            if alpha.len() == 3 && rest.starts_with(|c: char| c.is_ascii_digit()) {
                if let Some((_, name)) = AIRLINES.iter().find(|(p, _)| *p == alpha) {
                    return Some((*name).to_string());
                }
            }
        }
    }
    if military {
        return Some("Military".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::operator_for;

    #[test]
    fn airlines_match_flight_shape() {
        assert_eq!(operator_for(Some("UAL1381"), false).as_deref(), Some("United Airlines"));
        assert_eq!(operator_for(Some("aal2867"), false).as_deref(), Some("American Airlines"));
    }

    #[test]
    fn bare_tail_is_not_an_airline() {
        assert_eq!(operator_for(Some("N4872D"), false), None);
    }

    #[test]
    fn military_callsign_and_flag_fallback() {
        assert_eq!(operator_for(Some("RCH285"), true).as_deref(), Some("USAF · Air Mobility"));
        assert_eq!(operator_for(Some("BONE21"), true).as_deref(), Some("Military"));
        assert_eq!(operator_for(Some(""), true).as_deref(), Some("Military"));
        assert_eq!(operator_for(None, false), None);
    }
}

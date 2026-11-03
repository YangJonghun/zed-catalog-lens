//! In `auto` the server must know whether Zed draws code lenses above lines,
//! which Zed never says.
//! A lens request alone does not tell: Zed's `code_lens: "menu"` asks for
//! lenses on cursor moves to fill the code action menu. Only `"on"` answers
//! `workspace/codeLens/refresh` with a lens request, so the server probes.

use std::time::Duration;

use serde::Deserialize;

pub const LENS_PROBE_GRACE: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DisplayMode {
    Inlay,
    CodeLens,
    Both,
    #[serde(other)]
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintDecision {
    Show,
    Hide,
    Wait,
}

pub fn hint_decision(mode: DisplayMode, lenses: &LensTracker) -> HintDecision {
    match mode {
        DisplayMode::Inlay | DisplayMode::Both => HintDecision::Show,
        DisplayMode::CodeLens => HintDecision::Hide,
        DisplayMode::Auto => match lenses.belief {
            Belief::Unknown => HintDecision::Wait,
            Belief::Settled(found) | Belief::Doubted(found) => match found {
                ClientLenses::Inline => HintDecision::Hide,
                ClientLenses::NotInline => HintDecision::Show,
            },
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientLenses {
    Inline,
    NotInline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Belief {
    Unknown,
    Settled(ClientLenses),
    Doubted(ClientLenses),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LensEvent {
    /// `workspace/didChangeConfiguration`, which Zed sends on every settings
    /// change, `code_lens` toggles included.
    SettingsChanged,
    LensRequested,
    HintsRequested,
    ProbeExpired(ProbeId),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub start_probe: Option<ProbeId>,
    pub refresh_hints: bool,
}

/// Zed's setting covers every buffer, so one tracker serves all documents.
#[derive(Debug)]
pub struct LensTracker {
    belief: Belief,
    probe: Option<ProbeId>,
    probes_started: u64,
}

impl Default for LensTracker {
    fn default() -> Self {
        Self {
            belief: Belief::Unknown,
            probe: None,
            probes_started: 0,
        }
    }
}

impl LensTracker {
    pub fn on(&mut self, event: LensEvent) -> Effects {
        match event {
            LensEvent::SettingsChanged => {
                if let Belief::Settled(found) = self.belief {
                    self.belief = Belief::Doubted(found);
                }
                let start_probe = self.probe.is_some().then(|| self.start_probe());
                Effects {
                    start_probe,
                    refresh_hints: true,
                }
            }
            LensEvent::LensRequested if self.probe.is_some() => self.settle(ClientLenses::Inline),
            LensEvent::LensRequested | LensEvent::HintsRequested => {
                let unsure = !matches!(self.belief, Belief::Settled(_));
                let start_probe = (unsure && self.probe.is_none()).then(|| self.start_probe());
                Effects {
                    start_probe,
                    refresh_hints: false,
                }
            }
            LensEvent::ProbeExpired(id) if self.probe == Some(id) => {
                self.settle(ClientLenses::NotInline)
            }
            LensEvent::ProbeExpired(_) => Effects::default(),
        }
    }

    fn start_probe(&mut self) -> ProbeId {
        self.probes_started += 1;
        let id = ProbeId(self.probes_started);
        self.probe = Some(id);
        id
    }

    fn settle(&mut self, found: ClientLenses) -> Effects {
        let answered = match self.belief {
            Belief::Unknown => None,
            Belief::Settled(previous) | Belief::Doubted(previous) => Some(previous),
        };
        self.belief = Belief::Settled(found);
        self.probe = None;
        Effects {
            start_probe: None,
            refresh_hints: answered.is_some_and(|previous| previous != found),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTO: DisplayMode = DisplayMode::Auto;

    fn probe(effects: Effects) -> ProbeId {
        effects.start_probe.expect("a probe starts")
    }

    fn settled(answer: impl FnOnce(&mut LensTracker, ProbeId) -> Effects) -> LensTracker {
        let mut lenses = LensTracker::default();
        let id = probe(lenses.on(LensEvent::HintsRequested));
        answer(&mut lenses, id);
        lenses
    }

    fn inline() -> LensTracker {
        settled(|lenses, _| lenses.on(LensEvent::LensRequested))
    }

    fn not_inline() -> LensTracker {
        settled(|lenses, id| lenses.on(LensEvent::ProbeExpired(id)))
    }

    #[test]
    fn fixed_modes_ignore_the_tracker() {
        for lenses in [LensTracker::default(), inline(), not_inline()] {
            assert_eq!(
                hint_decision(DisplayMode::Inlay, &lenses),
                HintDecision::Show
            );
            assert_eq!(
                hint_decision(DisplayMode::Both, &lenses),
                HintDecision::Show
            );
            assert_eq!(
                hint_decision(DisplayMode::CodeLens, &lenses),
                HintDecision::Hide
            );
        }
    }

    #[test]
    fn hints_wait_only_for_the_first_probe() {
        let mut lenses = LensTracker::default();
        let id = probe(lenses.on(LensEvent::HintsRequested));
        assert_eq!(hint_decision(AUTO, &lenses), HintDecision::Wait);
        assert_eq!(lenses.on(LensEvent::HintsRequested), Effects::default());
        assert_eq!(
            lenses.on(LensEvent::ProbeExpired(id)),
            Effects::default(),
            "waiting requests get the answer directly"
        );
        assert_eq!(hint_decision(AUTO, &lenses), HintDecision::Show);
    }

    #[test]
    fn only_a_lens_request_during_a_probe_means_inline() {
        let mut lenses = LensTracker::default();
        let id = probe(lenses.on(LensEvent::LensRequested));
        assert_eq!(
            hint_decision(AUTO, &lenses),
            HintDecision::Wait,
            "the lens request that started the probe proves nothing"
        );
        lenses.on(LensEvent::LensRequested);
        assert_eq!(hint_decision(AUTO, &lenses), HintDecision::Hide);
        assert_eq!(lenses.on(LensEvent::ProbeExpired(id)), Effects::default());
        assert_eq!(hint_decision(AUTO, &lenses), HintDecision::Hide);
    }

    #[test]
    fn settled_beliefs_ignore_requests() {
        for mut lenses in [inline(), not_inline()] {
            let before = hint_decision(AUTO, &lenses);
            for event in [LensEvent::LensRequested, LensEvent::HintsRequested] {
                assert_eq!(lenses.on(event), Effects::default(), "{event:?}");
            }
            assert_eq!(hint_decision(AUTO, &lenses), before);
        }
    }

    #[test]
    fn a_settings_change_reprobes_on_the_next_request_and_refreshes_on_change() {
        let mut lenses = inline();
        assert_eq!(
            lenses.on(LensEvent::SettingsChanged),
            Effects {
                start_probe: None,
                refresh_hints: true
            }
        );
        assert_eq!(
            hint_decision(AUTO, &lenses),
            HintDecision::Hide,
            "the old answer stands while doubted"
        );
        let id = probe(lenses.on(LensEvent::HintsRequested));
        assert_eq!(
            lenses.on(LensEvent::ProbeExpired(id)),
            Effects {
                start_probe: None,
                refresh_hints: true
            }
        );
        assert_eq!(hint_decision(AUTO, &lenses), HintDecision::Show);

        lenses.on(LensEvent::SettingsChanged);
        probe(lenses.on(LensEvent::LensRequested));
        assert!(lenses.on(LensEvent::LensRequested).refresh_hints);
        assert_eq!(hint_decision(AUTO, &lenses), HintDecision::Hide);
    }

    #[test]
    fn an_unchanged_finding_refreshes_nothing() {
        let mut lenses = not_inline();
        lenses.on(LensEvent::SettingsChanged);
        let id = probe(lenses.on(LensEvent::LensRequested));
        assert_eq!(lenses.on(LensEvent::ProbeExpired(id)), Effects::default());
    }

    #[test]
    fn a_settings_change_during_a_probe_replaces_it() {
        let mut lenses = LensTracker::default();
        let old = probe(lenses.on(LensEvent::HintsRequested));
        let new = probe(lenses.on(LensEvent::SettingsChanged));
        assert_ne!(old, new);
        assert_eq!(lenses.on(LensEvent::ProbeExpired(old)), Effects::default());
        assert_eq!(hint_decision(AUTO, &lenses), HintDecision::Wait);
        lenses.on(LensEvent::ProbeExpired(new));
        assert_eq!(hint_decision(AUTO, &lenses), HintDecision::Show);
    }
}

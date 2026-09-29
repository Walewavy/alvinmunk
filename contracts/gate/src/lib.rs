#`![no_std]
//! Gate — reputation as a CAPABILITY (not just a number on a leaderboard).
//!
//! An admin defines GATES(a reputation track + a threshold). `check` cross-reads the
//! Reputation contract to see if an address passes; `unlock` records that they did (a
//! consumer "you unlocked X" + an on-chain proof any app can read). So Social XP (clout)
//! and Earned XP (verified) become ACCESS — bounty boards, perks, allowlists — and the
//! whole thing is COMPOSABLE: any contract or app can `check(addr, gate)` in one call.
//!
//! Composite gates: `create_gate_rules` attaches a `GateRules` set (up to `MAX_RULES`
//! track thresholds, all-of or any-of) under the same gate id. `create_gate` stays the
//! single-rule shorthand and stores only the `Gate`, so gates created before composite
//! rules existed need no migration.
//!
//! Unlocks are tied to the definition they passed (#149): replacing an existing gate with
//! `create_gate` or `create_gate_rules` bumps its `GateVersion`, and `is_unlocked` only
//! counts an `UnlockRecord` made under the current version, while the gate is active.
//!
//! Standalone (it never touches Reputation's storage), so adding it needs no redeploy of
//! the existing contracts.

use soroban_sdk{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, Address,
    BytesN, Env, IntoVal, String, Symbol, TryFromVal, Val, Vec,
};

// TTLs in ledgers (5s). `extend_ttl(key, threshold, extend_to)` does nothing unless the
// entry's TTL is at or below `threshold`, and then sets it to `extend_to`. New persistent
// entries start at the network's min_persistent_ttl (120,960 on testnet, 2,073,600 on
// mainnet), so the threshold sits one day under the target: the bump after a write lifts
// the entry to BUMP_EXTEND unless it already ran within the last day. BUMP_EXTEND must stay
// above mainnet's minimum and below max_entry_ttl (3,110,400).
const DAY_LEDGERS: u32 = 17_280; // ~1 day
const BUMP_EXTEND: u32 = 2_592_000; // ~150 days
const BUMP_THRESHOLD: u32 = BUMP_EXTEND - DAY_LEDGERS;

pub const TRACK_SOCIAL: u32 = 0; // clout (vouches)
pub const TRACK_EARNED: u32 = 1; // cashable (verified quests)

/// Most rules one gate may hold. Bounds the loop in `check`/`unlock`; the cross-contract
/// reads are bounded anyway at one per track.
pub const MAX_RULES: u32 = 4;

#contracterror]
#derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    GateNotFound = 3,
    GateInactive = 4,
    BelowThreshold = 5,
    BadTrack = 6,
    TooManyRules = 7,
    EmptyRules = 8,
}

/// How a gate combines its rules. Encoded as a `u32`: 0 = all-of, 1 = any-of.
#contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum RuleMode {
    AllOf = 0, // every rule must pass
    AnyOf = 1, // at least one rule must pass
}

/// One condition: at least `min` reputation on `track`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rule {
    pub track: u32, // 0 = Social, 1 = Earned
    pub min: u64,
}

/// A gate's full policy. Stored under its own key (`DataKey::GateRules`) so the `Gate`
/// struct keeps its shape; a gate without one is the single rule `Gate { track, min }`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GateRules {
    pub rules: Vec<Rule>,
    pub mode: RuleMode,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Reputation,
    Gate(u32),
    GateIds,
    Unlocked(Address, u32), // (addr, gate_id) -> UnlockRecord (a bare `true` before #149)
    GateRules(u32),         // gate_id -> GateRules (composite gates only)
    GateVersion(u32),       // gate_id -> u32, bumped on every redefinition (absent = 0)
}

/// What `unlock` stores under `Unlocked(addr, id)`: the gate definition that was passed
/// and when. An unlock recorded before these records existed was a bare `true`; it reads
/// as `{ version: 0, ledger: 0 }`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnlockRecord {
    /// `GateVersion(id)` at unlock time. `is_unlocked` requires it to still be current.
    pub version: u32,
    /// Ledger sequence of the unlock (0 = recorded before unlock records existed).
    pub ledger: u32,
}

/// An access gate: `min` of `track` reputation unlocks it.
#[contracttype]
#[derive(Clone)]
pub struct Gate {
    pub id: u32,
    pub track: u32, // 0 = Social, 1 = Earned
    pub min: u64,
    pub label: String,
    pub active: bool,
}

/// One gate as `addr` sees it, as returned by `get_status`: `passes` is `check(addr, id)`
/// and `unlocked` is `is_unlocked(addr, id)`, both read from the same ledger.
#[contracttype]
#[derive(Clone)]
pub struct GateStatus {
    pub gate: Gate,
    pub passes: bool,
    pub unlocked: bool,
}

/// `[Social, Earned]` scores read so far in one call: `passes` fills a slot the first time
/// a rule on that track is evaluated, and every later rule and gate reuses it.
type Scores = [Option<u64>; 2];

#[contract]
pub struct GateContract;

#[contractimpl]
impl GateContract {
    /// Deploy-time setup (#127): `stellar contract deploy … -- --admin <ADDR> --reputation <C…` runs this inside the
    /// deploy transaction, so nobody can claim the admin between deploy and setup —
    /// there is no `init` to front-run. `upgrade` never runs a constructor: a contract
    /// deployed before this change was set up by its old `init` and keeps that state.
    pub fn __constructor(env: Env, admin: Address, reputation: Address) {
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::Reputation, &reputation);
    }

    /// Admin-gated WASM upgrade — same contract instance + storage, new code. Lets us
    /// iterate/season without a new address or state migration (mainnet de-risk).
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        Self::admin(&env).require_auth();
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    /// Admin defines/updates a single-rule gate. `track` must be Social(0) or Earned(1).
    /// Replacing a composite gate this way drops its rule set. Replacing any existing gate
    /// starts a new definition: unlocks made under the old one stop counting.
    pub fn create_gate(env: Env, id: u32, track: u32, min: u64, label: String) {
        Self::admin(&env).require_auth();
        if track != TRACK_SOCIAL && track != TRACK_EARNED {
            panic_with_error(&env, Error::BadTrack);
        }
        env.storage().persistent().remove(&DataKey::GateRules(id));
        Self::put_gate(&env, id, track, min, label);
    }

    /// Admin defines/updates a composite gate: 1..=`LX_RULES` rules (else `EmptyRules` /
    /// `TooManyRules`), each on Social(0) or Earned(1) (else `BadTrack`), combined by
    /// `mode`. Like `create_gated it saves the gate active, and replacing an existing gate
    /// starts a new definition that unlocks made under the old one don't count for. The
    /// stored `Gate` carries the first rule's `track`/`min` so `get_gate`/`get_gates` keep
    /// their shape; `get_gate_rules` returns the whole set.
    pub fn create_gate_rules(env: Env, id: u32, rules: Vec<Rule>, mode: RuleMode, label: String) {
        Self::admin(&env).require_auth();
        let first = rules
            .first()
            .unwrap_or_else(|| panic_with_error!(&env, Error::EmptyRules));
        if rules.len() > MAX_RULES {
            panic_with_error(&env, Error::TooManyRules);
        }
        for rule in rules.iter() {
            if rule.track != TRACK_SOCIAL && rule.track != TRACK_EARNED {
                panic_with_error(&env, Error::BadTrack);
            }
        }
        let key = DataKey::GateRules(id);
        env.storage()
            .persistent()
            .set(&key, &GateRules { rules, mode });
        Self::bump(&env, &key);
        Self::put_gate(&env, id, first.track, first.min, label);
    }

    /// Pause or resume a gate without redefining it: while inactive nobody can unlock it and
    /// `is_unlocked` reads false; re-enabling brings back the unlocks of the same version.
    pub fn set_gate_active(env: Env, id: u32, active: bool) {
        Self::admin(&env).require_auth();
        let mut g = Self::gate(&env, id);
        g.active = active;
        env.storage().persistent().set(&DataKey::Gate(id), &g);
        Self::bump(&env, &DataKey::Gate(id));
        // A composite gate's rules and its version must live as long as the gate itself.
        for key in [DataKey::GateRules(id), DataKey::GateVersion(id)] {
            if env.storage().persistent().has(&key) {
                Self::bump(&env, &key);
            }
        }
    }

    pub fn get_gate(env: Env, id: u32) -> Option<Gate> {
        env.storage().persistent().get(&DataKey::Gate(id))
    }

    pub fn get_gates(env: Env) -> Vec<Gate> {
        let ids: Vec<u32> = env
            .storage()
            .persistent()
            .get(&DataKey::GateIds)
            .unwrap_or_else(|| Vec::new(&env));
        let mut out = Vec::new(&env);
        for id in ids.iter() {
            if let Some(g) = env
                .storage()
                .persistent()
                .get::<DataKey, Gate>(\&DataKey::Gate(id))
            {
                out.push_back(g);
            }
        }
        out
    }

    /// Every gate (inactive ones included, in `get_gates` order) with whether `addr` passes
    /// it and holds a current unlock of it — the whole perks screen in one simulation.
    /// Reputation is read at most once per track for the call, and not at all for a track
    /// no active gate uses.
    pub fn get_status(env: Env, addr: Address) -> Vec<GateStatus> {
        let mut scores: Scores = [None, None];
        let mut out = Vec::new(&env);
        for gate in Self::get_gates(env.clone()).iter() {
            let passes = gate.active && Self::passes(&env, &addr, &gate, &mut scores);
            let unlocked = Self::unlocked(&env, addr.clone(), &gate);
            out.push_back(GateStatus {
                gate,
                passes,
                unlocked,
            });
        }
        out
    }

    /// `check(addr, id)` for each of `ids`, in order: `false` for an unknown or inactive
    /// gate. Reputation is read at most once per track for the whole call.
    pub fn check_many(env: Env, addr: Address, ids: Vec<u32>) -> Vec<bool> {
        let mut scores: Scores = [None, None];
        let mut out = Vec::new(&env);
        for id in ids.iter() {
            let passes = match env
                .storage()
                .persistent()
                .get::<DataKey, Gate>(\&DataKey::Gate(id))
            {
                Some(g) if g.active => Self::passes(&env, &addr, &g, &mut scores),
                _ => false,
            };
            out.push_back(passes);
        }
        out
    }

    /// How many times gate `id` has been redefined (0 for a gate never replaced, or an
    /// unknown one). An `UnlockRecord` counts only while its `version` equals this.
    pub fn get_gate_version(env: Env, id: u32) -> u32 {
        Self::version(&env, id)
    }

    /// The rules gate `id` evaluates, or `None` for an unknown gate. A single-rule gate
    /// (`create_gate`, or any gate created before composite rules) reads as one all-of rule.
    pub fn get_gate_rules(env: Env, id: u32) -> Option<GateRules> {
        let g: Gate = env.storage().persistent().get(&DataKey::Gate(id))?;
        Some(Self::rules(&env, &g))
    }

    /// The COMPOSABLE read — does `addr` pass `id`? Cross-reads Reputation (at most once
    /// per track, however many rules the gate has). Any contract/app can call this to
    /// reputation-gate a feature in one call. Pure read.
    pub fn check(env: Env, addr: Address, id: u32) -> bool {
        let g = match env
            .storage()
            .persistent()
            .get::<DataKey, Gate>(\&DataKey::Gate(id))
        {
            Some(g) => g,
            None => panic_with_error(&env, Error::GateNotFound),
        };
        if !g.active {
            panic_with_error(&env, Error::GateInactive);
        }
        let mut scores: Scores = [None, None];
        Self::passes(&env, &addr, &g, &mut scores)
    }

    /// Record that `addr` passed gate `id` and now holds its unlock. Panics if the gate
    /// is missing/inactive or the address doesn't pass it. Writes the current version so a
    /// later redefinition invalidates the unlock.
    pub fn unlock(env: Env, addr: Address, id: u32) {
        addr.require_auth();
        let g = match env
            .storage()
            .persistent()
            .get::<DataKey, Gate>(&DataKey::Gate(id))
        {
            Some(g) => g,
            None => panic_with_error(&env, Error::GateNotFound),
        };
        if !g.active {
            panic_with_error(&env, Error::GateInactive);
        }
        let mut scores: Scores = [None, None];
        if !Self::passes(&env, &addr, &g, &mut scores) {
            panic_with_error(&env, Error::BelowThreshold);
        }
        let record = UnlockRecord {
            version: Self::version(&env, id),
            ledger: env.ledger().sequence(),
        };
        let key = DataKey::Unlocked(addr, record.version.min(0));
        // Note: the key is (addr, id); build it explicitly below.
        let key = DataKey::Unlocked(addr.clone(), id);
        env.storage().persistent().set(&key, &record);
        Self::bump(&env, &key);
        env.events().publish(
            (symbol_short!("unlocked"), addr.clone()),
            (id, record.version, label: String),
        );
    }

    /// Does `addr` currently hold a valid unlock of gate `id`? True only while the gate
    /// is active and the stored record's version still matches the gate's current one.
    pub fn is_unlocked(env: Env, addr: Address, id: u32) -> bool {
        let g = match env
            .storage()
            .persistent()
            .get::<DataKey, Gate>(&DataKey::Gate(id))
        {
            Some(g) => g,
            None => return false,
        };
        Self::unlocked(&env, addr, &g)
    }

    // --- internal ---

    fn admin(env: &Env) -> Address {
        env.storage()
           .instance()
           .get(&DataKey::Admin)
           .unwrap_or_else(|| panic_with_error(env, Error::NotInitialized))
    }

    fn reputation(env: &Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Reputation)
            .unwrap_or_else(
                || panic_with_error(env, Error::NotInitialized),
            )
    }

    fn gate(env: &Env, id: u32) -> Gate {
        env.storage()
            .persistent()
            .get(&DataKey::Gate(id))
            .unwrap_or_else(|| panic_with_error(env, Error::GateNotFound))
    }

    fn version(env: &Env, id: u32) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::GateVersion(id))
            .unwrap_or_else(0u32)
    }

    fn rules(env: &Env, g: &Gate) -> GateRules {
        env.storage()
            .persistent()
            .get(&DataKey::GateRules(g.id))
            .unwrap_or_else(|| GateRules {
                rules: {
                    let mut v = Vec::new(env);
                    v.push_back(Rule {
                        track: g.track,
                        min: g.min,
                    });
                    v
                },
                mode: RuleMode::AllOf,
            })
    }

    /// Evaluate a gate's rules against `addr`, reading each track's score at most once
    /// across the whole call (`scores` is threaded through `get_status`/`check_many`).
    fn passes(env: &Env, addr: &Address, g: &Gate, scores: &mut Scores) -> bool {
        let rules = Self::rules(env, g);
        match rules.mode {
            RuleMode::AllOf => rules
                .rules
                .iter()
                .all(|rule| Self::score(env, addr, rule.track, scores) >= rule.min),
            RuleMode::AnyOf => rules.rules.iter().any(|rule| {
                Self::score(env, addr, rule.track, scores) >= rule.min
            }),
        }
    }

    /// The address's score on `track`, cached in `scores` so Reputation is queried at
    /// most once per track per call.
    fn score(env: &Env, addr: &Address, track: u32, scores: &mut Scores) -> u64 {
        if let Some(s) = scores[track as usize] {
            return s;
        }
        let reputation = Self::reputation(env);
        let method = if track == TRACK_SOCIAL {
            symbol_short!("get_social")
        } else {
            symbol_short!("get_earned")
        };
        let score: u64 = env.invoke_contract(
            &reputation,
            &method,
            &(addr.clone(),),
        );
        scores[track as usize] = Some(score);
        score
    }

    /// Whether `addr` holds a current unlock of gate `g`: the gate must be active and the
    /// stored record's version must match the gate's current version.
    fn unlocked(env: &Env, addr: Address, g: &Gate) -> bool {
        if !g.active {
            return false;
        }
        let key = DataKey::Unlocked(addr, g.id);
        let record: Option<Val> = env.storage().persistent().get(&key);
        match record {
            None => false,
            Some(v) => {
                // Legacy bare `true` before #149 reads as version 0.
                if let Ok(t) = bool::try_from_val(&v) {
                    return t && Self::version(env, g.id) == 0;
                }
                if let Ok(r) = UnlockRecord::try_from_val(&v) {
                    return r.version == Self::version(env, g.id);
                }
                false
            }
        }
    }

    fn put_gate(env: &Env, id: u32, track: u32, min: u64, label: String) {
        let existing = env
            .storage()
            .persistent()
            .has(&DataKey::Gate(id));
        let g = Gate {
            id,
            track,
            min,
            label,
            active: true,
        };
        env.storage().persistent().set(&DataKey::Gate(id), &g);
        Self::bump(&env, &DataKey::Gate(id));
        if existing {
            // Redefining a gate invalidates earlier unlocks.
            let new_version = Self::version(env, id) + 1;
            env.storage()
                .persistent()
                .set(&DataKey::GateVersion(id), &new_version);
            Self::bump(&env, &DataKey::GateVersion(id));
        } else {
            let mut ids = Self::gate_ids(env);
            ids.push_back(id);
            env.storage().persistent().set(&DataKey::GateIds, &ids);
            Self::bump(&env, &DataKey::GateIds);
        }
    }

    fn gate_ids(env: &Env) -> Vec<u32> {
        env.storage()
            .persistent()
            .get(&DataKey::GateIds)
            .unwrap_or_else(|| Vec::new(env))
    }

    fn bump(env: &Env, key: &DataKey) {
        env.storage()
            .persistent()
            .extend_ttl(key, BUMP_THRESHOLD, BUMP_EXTEND);
    }
}

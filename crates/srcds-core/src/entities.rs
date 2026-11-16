//! Networked entities served by srcds-rs: class / send-table definitions, per-tick
//! snapshots and their `svc_PacketEntities` encoding.
//!
//! The tables below are srcds-rs' own descriptions. Table and property *names* and
//! value *types* must exist in the client's receive tables (learned from the client's
//! own demo data-table dump, see `docs/PROTOCOL.md`); encodings (bit widths, flags) are
//! chosen here — mostly raw 32-bit floats and fixed-width integers — because the client
//! decodes with whatever the server declares. Only a small subset of each class is
//! networked; everything else keeps its client-side default.

use srcds_protocol::bitbuf::BitWriter;
use srcds_protocol::error::Result;
use srcds_protocol::sendtable::{
    self, EntityUpdate, FlatProp, PropValue, SendProp, SendTable, ServerClass, TableSet,
};

use crate::bsp::MapInfo;

pub const CLASS_WORLD: u32 = 0;
pub const CLASS_PLAYER: u32 = 1;
pub const CLASS_PLAYER_RESOURCE: u32 = 2;
pub const CLASS_PHYSICS_PROP: u32 = 3;
pub const CLASS_VIEWMODEL: u32 = 4;
/// First weapon class; see [`WEAPON_CLASSES`].
pub const CLASS_WEAPON_FIRST: u32 = 5;

/// Weapon entity classes: (Lua class, server class, send table). Scripted weapons use
/// the last entry (`CWeaponSWEP`).
pub const WEAPON_CLASSES: &[(&str, &str, &str)] = &[
    ("weapon_physgun", "CWeaponPhysGun", "DT_WeaponPhysGun"),
    ("weapon_physcannon", "CWeaponPhysCannon", "DT_WeaponPhysCannon"),
    ("weapon_crowbar", "CWeaponCrowbar", "DT_WeaponCrowbar"),
    ("weapon_pistol", "CWeaponPistol", "DT_WeaponPistol"),
    ("weapon_smg1", "CWeaponSMG1", "DT_WeaponSMG1"),
    ("weapon_ar2", "CWeaponAR2", "DT_WeaponAR2"),
    ("weapon_shotgun", "CWeaponShotgun", "DT_WeaponShotgun"),
    ("weapon_357", "CWeapon357", "DT_Weapon357"),
    ("weapon_crossbow", "CWeaponCrossbow", "DT_WeaponCrossbow"),
    ("weapon_rpg", "CWeaponRPG", "DT_WeaponRPG"),
    ("weapon_frag", "CWeaponFrag", "DT_WeaponFrag"),
    ("", "CWeaponSWEP", "DT_WeaponSWEP"),
];
/// Reserve-ammo slots networked (ids 1.. of the ammo types; 37 built in).
pub const AMMO_SLOTS: usize = 64;

/// Slots per NetworkVar type.
pub const DT_SLOTS: usize = 32;

/// One NetworkVar value: (type, slot, value). Types as GMod's `DTVar` order:
/// 0 bool, 1 int, 2 float, 3 Vector, 4 Angle, 5 Entity (handle), 6 string (slots 0–3).
pub type DtVar = (u8, i32, PropValue);

/// Writes NetworkVars into an entity; unused entity slots are "no entity".
pub fn set_dtvars(nc: &NetClasses, e: &mut EntityState, vars: &[DtVar]) {
    let names = element_names();
    for name in &names[..DT_SLOTS] {
        e.set(nc, "m_GMOD_EHANDLE", name, PropValue::Int(INVALID_EHANDLE));
    }
    for (ty, slot, v) in vars {
        let Ok(i) = usize::try_from(*slot) else { continue };
        if *ty == 6 {
            const STRINGS: [&str; 4] = ["m_GMOD_String0", "m_GMOD_String1", "m_GMOD_String2", "m_GMOD_String3"];
            if let (Some(n), PropValue::String(_)) = (STRINGS.get(i), v) {
                e.set(nc, "DT_BaseEntity", n, v.clone());
            }
            continue;
        }
        if i >= DT_SLOTS {
            continue;
        }
        let table = match ty {
            0 => "m_GMOD_bool",
            1 => "m_GMOD_int",
            2 => "m_GMOD_float",
            3 => "m_GMOD_Vector",
            4 => "m_GMOD_QAngle",
            5 => "m_GMOD_EHANDLE",
            _ => continue,
        };
        let ok = matches!(
            (ty, v),
            (0 | 1 | 5, PropValue::Int(_)) | (2, PropValue::Float(_)) | (3 | 4, PropValue::Vector(_))
        );
        if ok {
            e.set(nc, table, names[i], v.clone());
        }
    }
}

/// Weapon slots networked in `m_hMyWeapons`.
pub const MAX_WEAPONS: usize = 48;
/// `m_iState` of a weapon.
pub const WEAPON_NOT_CARRIED: i32 = 0;
pub const WEAPON_IS_CARRIED: i32 = 1;
pub const WEAPON_IS_ACTIVE: i32 = 2;
/// `m_fEffects` bits.
pub const EF_BONEMERGE: i32 = 0x001;
pub const EF_NOSHADOW: i32 = 0x010;
pub const EF_NODRAW: i32 = 0x020;
pub const EF_BONEMERGE_FASTCULL: i32 = 0x080;

/// Model entities without physics or AI here (dynamic props, NPC models, ragdolls).
pub const CLASS_DYNAMIC_PROP: u32 = CLASS_WEAPON_FIRST + WEAPON_CLASSES.len() as u32;
/// Scripted Lua entities of type "anim" (`CSENT_anim`).
pub const CLASS_SENT_ANIM: u32 = CLASS_DYNAMIC_PROP + 1;
/// Visual ropes (`CRopeKeyframe`).
pub const CLASS_ROPE: u32 = CLASS_SENT_ANIM + 1;
/// Map brush entities (`func_brush`, `func_wall`, doors, …): [факт] the reference
/// networks them as plain `CBaseEntity`.
pub const CLASS_BRUSH: u32 = CLASS_ROPE + 1;
/// HL2 useable ladders (`func_useableladder` → `CFuncLadder`).
pub const CLASS_LADDER: u32 = CLASS_BRUSH + 1;
/// The physgun beam (`CPhysBeam`).
pub const CLASS_PHYSBEAM: u32 = CLASS_LADDER + 1;

/// Class id of the weapon class for a Lua class name (`scripted` = Lua SWEP).
pub fn weapon_class(lua_class: &str, scripted: bool) -> u32 {
    let last = WEAPON_CLASSES.len() - 1;
    let i = if scripted {
        last
    } else {
        WEAPON_CLASSES[..last].iter().position(|w| w.0 == lua_class).unwrap_or(last)
    };
    CLASS_WEAPON_FIRST + i as u32
}

/// Player slots covered by the player-resource arrays (`000`..`128`; slot `n` is
/// element `n + 1`, element 0 is unused).
pub const PLAYER_RESOURCE_SLOTS: usize = 129;
/// Per-player arrays of the player resource, with the integer width srcds-rs uses.
const PLAYER_RESOURCE_ARRAYS: &[(&str, u32, bool)] = &[
    ("m_iPing", 12, true),
    ("m_iScore", 32, false),
    ("m_iDeaths", 32, false),
    ("m_bConnected", 1, true),
    ("m_iTeam", 32, false),
    ("m_bAlive", 1, true),
    ("m_iHealth", 32, false),
    ("m_iArmor", 32, false),
];
/// GMod's default team for players (`TEAM_UNASSIGNED`).
pub const TEAM_UNASSIGNED: i32 = 1001;

/// Element names `"000"`, `"001"`, … of the player-resource arrays.
fn element_names() -> &'static [&'static str] {
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        (0..PLAYER_RESOURCE_SLOTS)
            .map(|i| &*Box::leak(format!("{i:03}").into_boxed_str()))
            .collect()
    })
}

/// `movetype` values.
pub const MOVETYPE_WALK: i32 = 2;
pub const MOVETYPE_NOCLIP: i32 = 8;
pub const MOVETYPE_NONE: i32 = 0;
pub const MOVETYPE_LADDER: i32 = 9;
/// `m_fFlags`: on ground.
pub const FL_ONGROUND: i32 = 1 << 0;
/// `m_fFlags`: entity is a player. [факт] bit 8 in GMod (reference player baseline
/// carries `m_fFlags = 256`, 257 when on ground).
pub const FL_CLIENT: i32 = 1 << 8;
/// Default field of view announced for players (`m_iDefaultFOV`); `m_iFOV = 0`
/// means "use the default". Without it the client's FOV-based mouse scaling breaks.
pub const DEFAULT_FOV: f32 = 75.0;
/// Networked entity handle (`index | serial << 13`, 23 bits) meaning "none".
pub const INVALID_EHANDLE: i32 = (1 << 23) - 1;

/// Simulation-time encoding: ticks relative to a base that advances in steps of
/// 100 ticks, offset per entity by `index % 32` (the client reconstructs the absolute
/// tick from its own clock with the same rule).
const TIMESTAMP_BASE: i32 = 100;
const TIMESTAMP_WINDOW: i32 = 32;

pub fn ehandle(index: u32, serial: u32) -> i32 {
    (index as i32 & 0x1FFF) | (((serial & 0x3FF) as i32) << 13)
}

/// Encoded `m_flSimulationTime` for an entity simulated at `sim_tick`.
pub fn simulation_time_value(sim_tick: i32, index: u32) -> i32 {
    let m = index as i32 % TIMESTAMP_WINDOW;
    let base = TIMESTAMP_BASE * (sim_tick - m).div_euclid(TIMESTAMP_BASE);
    (sim_tick - base) & 0xFF
}

fn int_u(name: &'static str, bits: u32) -> SendProp {
    SendProp::int(name, bits, true)
}

fn int_s(name: &'static str, bits: u32) -> SendProp {
    SendProp::int(name, bits, false)
}

/// srcds-rs' send tables and classes.
pub fn tables() -> (Vec<ServerClass>, TableSet) {
    let t = |name, props| SendTable { name, props };
    let tables = vec![
        t(
            "DT_BaseEntity",
            vec![
                int_u("m_flSimulationTime", 8),
                SendProp::vector("m_vecOrigin"),
                int_u("m_nModelIndex", 14),
                SendProp::vector("m_angRotation"),
                int_u("movetype", 4),
                int_s("m_iTeamNum", 16),
                int_u("m_CollisionGroup", 5),
                int_s("m_iHealth", 32),
                int_s("m_iMaxHealth", 32),
                SendProp::float("m_vecVelocity[0]"),
                SendProp::float("m_vecVelocity[1]"),
                SendProp::float("m_vecVelocity[2]"),
                SendProp::table("m_Collision", "DT_CollisionProperty"),
                int_u("m_nRenderFX", 8),
                int_u("m_nRenderMode", 8),
                int_u("m_fEffects", 16),
                int_u("m_clrRender", 32),
                int_u("m_hOwnerEntity", 23),
                int_u("moveparent", 23),
                // GMod NetworkVar / DTVar slots ([факт] reference send tables: six
                // 32-slot tables in DT_BaseEntity plus four strings)
                SendProp::table("m_GMOD_bool", "m_GMOD_bool"),
                SendProp::table("m_GMOD_float", "m_GMOD_float"),
                SendProp::table("m_GMOD_int", "m_GMOD_int"),
                SendProp::table("m_GMOD_Vector", "m_GMOD_Vector"),
                SendProp::table("m_GMOD_QAngle", "m_GMOD_QAngle"),
                SendProp::table("m_GMOD_EHANDLE", "m_GMOD_EHANDLE"),
                SendProp::string("m_GMOD_String0"),
                SendProp::string("m_GMOD_String1"),
                SendProp::string("m_GMOD_String2"),
                SendProp::string("m_GMOD_String3"),
            ],
        ),
        t(
            "DT_CollisionProperty",
            vec![
                SendProp::vector("m_vecMinsPreScaled"),
                SendProp::vector("m_vecMaxsPreScaled"),
                SendProp::vector("m_vecMins"),
                SendProp::vector("m_vecMaxs"),
                int_u("m_nSolidType", 3),
            ],
        ),
        t(
            "DT_WORLD",
            vec![
                SendProp::baseclass("DT_BaseEntity"),
                SendProp::vector("m_WorldMins"),
                SendProp::vector("m_WorldMaxs"),
            ],
        ),
        t(
            "DT_BaseAnimating",
            vec![
                SendProp::baseclass("DT_BaseEntity"),
                int_u("m_nSkin", 10),
                int_s("m_nBody", 32),
                int_u("m_nSequence", 12),
                SendProp::float("m_flPlaybackRate"),
                // [факт] reference DT_BaseAnimating: 2-bit muzzle flash counter
                int_u("m_nMuzzleFlashParity", 2),
                // players animate on the client ([факт] without it the client drew
                // the player model in its reference pose)
                int_u("m_bClientSideAnimation", 1),
            ],
        ),
        t(
            "DT_BreakableProp",
            vec![SendProp::baseclass("DT_BaseAnimating")],
        ),
        t(
            "DT_PhysicsProp",
            vec![SendProp::baseclass("DT_BreakableProp"), int_u("m_bAwake", 1)],
        ),
        t(
            "DT_BaseAnimatingOverlay",
            vec![SendProp::baseclass("DT_BaseAnimating")],
        ),
        t(
            "DT_BaseFlex",
            vec![
                SendProp::baseclass("DT_BaseAnimatingOverlay"),
                SendProp::float("m_vecViewOffset[0]"),
                SendProp::float("m_vecViewOffset[1]"),
                SendProp::float("m_vecViewOffset[2]"),
            ],
        ),
        t(
            "DT_BaseCombatCharacter",
            vec![
                SendProp::baseclass("DT_BaseFlex"),
                int_u("m_hActiveWeapon", 23),
                // [факт] reference server.dll: `bcc_localdata` (local player only there)
                SendProp::table("bcc_localdata", "DT_BCCLocalPlayerExclusive"),
                SendProp::table("m_hMyWeapons", "m_hMyWeapons"),
            ],
        ),
        t(
            "DT_BCCLocalPlayerExclusive",
            // [факт] reference: a time stored as a double (sizeof 8)
            vec![SendProp::double("m_flNextAttack")],
        ),
        t(
            "m_hMyWeapons",
            element_names()[..MAX_WEAPONS].iter().map(|n| int_u(n, 23)).collect(),
        ),
        t(
            "DT_LocalWeaponData",
            vec![
                int_s("m_iClip1", 16),
                int_s("m_iClip2", 16),
                int_u("m_nViewModelIndex", 4),
                int_s("m_iPrimaryAmmoType", 9),
                int_s("m_iSecondaryAmmoType", 9),
            ],
        ),
        // [факт] reference: these times are doubles (VectorXY with the double's words)
        t(
            "DT_LocalActiveWeaponData",
            vec![
                SendProp::double("m_flNextPrimaryAttack"),
                SendProp::double("m_flNextSecondaryAttack"),
                SendProp::double("m_flTimeWeaponIdle"),
            ],
        ),
        t(
            "DT_BaseCombatWeapon",
            vec![
                SendProp::baseclass("DT_BaseAnimating"),
                SendProp::table("LocalWeaponData", "DT_LocalWeaponData"),
                SendProp::table("LocalActiveWeaponData", "DT_LocalActiveWeaponData"),
                int_u("m_iViewModelIndex", 14),
                int_u("m_iWorldModelIndex", 14),
                int_u("m_iState", 4),
                int_u("m_hOwner", 23),
            ],
        ),
        t("DT_WeaponHL2MPBase", vec![SendProp::baseclass("DT_BaseCombatWeapon")]),
        t("DT_BaseHL2MPCombatWeapon", vec![SendProp::baseclass("DT_WeaponHL2MPBase")]),
        t("DT_BaseHL2MPBludgeonWeapon", vec![SendProp::baseclass("DT_BaseHL2MPCombatWeapon")]),
        // [факт] reference tables: the machine-gun shot counter and the pistol's
        // refire / accuracy state (both predicted by the client)
        t("DT_HL2MPMachineGun", vec![SendProp::baseclass("DT_BaseHL2MPCombatWeapon"), int_s("m_nShotsFired", 16)]),
        t(
            "DT_WeaponPhysGun",
            vec![
                SendProp::baseclass("DT_BaseHL2MPCombatWeapon"),
                int_u("m_hPhysBeam", 23),
                SendProp::vector("m_vHitPosLocal"),
                int_u("m_hGrabbedEntity", 23),
            ],
        ),
        // [факт] reference: the physgun's beam is its own entity
        t(
            "DT_PhysBeam",
            vec![
                SendProp::baseclass("DT_BaseEntity"),
                int_u("m_hTargetEnt", 23),
                SendProp::vector("m_HoldPos"),
                int_u("m_bIsOn", 1),
                int_s("m_iPhysBone", 12),
            ],
        ),
        t(
            "DT_WeaponPhysCannon",
            vec![
                SendProp::baseclass("DT_BaseHL2MPCombatWeapon"),
                int_u("m_bActive", 1),
                int_u("m_hAttachedObject", 23),
                int_s("m_EffectState", 4),
                int_u("m_bOpen", 1),
                int_u("m_bPhyscannonState", 1),
            ],
        ),
        t("DT_WeaponCrowbar", vec![SendProp::baseclass("DT_BaseHL2MPBludgeonWeapon")]),
        t(
            "DT_WeaponPistol",
            vec![
                SendProp::baseclass("DT_HL2MPMachineGun"),
                SendProp::double("m_flSoonestPrimaryAttack"),
                SendProp::double("m_flLastAttackTime"),
                SendProp::float("m_flAccuracyPenalty"),
                int_s("m_nNumShotsFired", 16),
            ],
        ),
        t("DT_WeaponSMG1", vec![SendProp::baseclass("DT_HL2MPMachineGun")]),
        t("DT_WeaponAR2", vec![SendProp::baseclass("DT_HL2MPMachineGun")]),
        // [факт] reference: the shotgun's pump / delayed fire / delayed reload flags
        t(
            "DT_WeaponShotgun",
            vec![
                SendProp::baseclass("DT_BaseHL2MPCombatWeapon"),
                int_u("m_bNeedPump", 1),
                int_u("m_bDelayedFire1", 1),
                int_u("m_bDelayedFire2", 1),
                int_u("m_bDelayedReload", 1),
            ],
        ),
        t("DT_Weapon357", vec![SendProp::baseclass("DT_BaseHL2MPCombatWeapon")]),
        // [факт] reference: scope and must-reload flags
        t(
            "DT_WeaponCrossbow",
            vec![SendProp::baseclass("DT_BaseHL2MPCombatWeapon"), int_u("m_bInZoom", 1), int_u("m_bMustReload", 1)],
        ),
        t("DT_WeaponRPG", vec![SendProp::baseclass("DT_BaseHL2MPCombatWeapon")]),
        t("DT_WeaponFrag", vec![SendProp::baseclass("DT_BaseHL2MPCombatWeapon")]),
        t("DT_ScriptedEntity", vec![SendProp::string("m_strScriptName")]),
        t(
            "DT_WeaponSWEP",
            vec![
                SendProp::baseclass("DT_BaseHL2MPCombatWeapon"),
                SendProp::table("m_ScriptedEntity", "DT_ScriptedEntity"),
                SendProp::string("m_strHoldType"),
            ],
        ),
        t(
            "DT_BaseViewModel",
            vec![
                int_u("m_nModelIndex", 14),
                int_s("m_nBody", 32),
                int_u("m_nSkin", 10),
                int_u("m_nSequence", 12),
                int_u("m_nViewModelIndex", 4),
                int_u("m_fEffects", 10),
                int_u("m_nAnimationParity", 3),
                int_u("m_hWeapon", 23),
                int_u("m_hOwner", 23),
                int_u("m_nNewSequenceParity", 3),
                // [факт] reference DT_BaseViewModel
                SendProp::float("m_flPlaybackRate"),
                int_u("m_nMuzzleFlashParity", 2),
            ],
        ),
        t("DT_PredictedViewModel", vec![SendProp::baseclass("DT_BaseViewModel")]),
        t(
            "DT_RopeKeyframe",
            vec![
                int_u("m_hStartPoint", 23),
                int_u("m_hEndPoint", 23),
                int_u("m_iStartBone", 5),
                int_u("m_iEndBone", 5),
                SendProp::vector("m_vecStartOffset"),
                SendProp::vector("m_vecEndOffset"),
                int_u("m_clrRender", 32),
                int_u("m_RopeLength", 15),
                int_u("m_fLockedPoints", 4),
                int_u("m_RopeFlags", 9),
                int_u("m_nSegments", 4),
                int_u("m_iRopeMaterialModelIndex", 16),
                int_u("m_Subdiv", 4),
                SendProp::float("m_TextureScale"),
                SendProp::float("m_Width"),
                SendProp::vector("m_vecOrigin"),
            ],
        ),
        t(
            "DT_DynamicProp",
            vec![SendProp::baseclass("DT_BreakableProp")],
        ),
        t(
            "DT_SENT_anim",
            vec![
                SendProp::baseclass("DT_BaseAnimatingOverlay"),
                SendProp::table("m_ScriptedEntity", "DT_ScriptedEntity"),
            ],
        ),
        t(
            "DT_Local",
            vec![
                int_u("m_iHideHUD", 12),
                int_u("m_bDrawViewmodel", 1),
                // HL2 HUD elements (health, ammo, weapon selection) need the suit
                int_u("m_bWearingSuit", 1),
                SendProp::float("m_flStepSize"),
                int_u("m_bAllowAutoMovement", 1),
                SendProp::float("m_flFallVelocity"),
                SendProp::vector("m_vecPunchAngle"),
                SendProp::vector("m_vecPunchAngleVel"),
                int_u("m_bDucked", 1),
                int_u("m_bDucking", 1),
                int_u("m_bInDuckJump", 1),
                SendProp::float("m_flDucktime"),
                SendProp::float("m_fSprintSpeed"),
                SendProp::float("m_fWalkSpeed"),
                SendProp::float("m_fSlowWalkSpeed"),
                SendProp::float("m_fLadderSpeed"),
                SendProp::float("m_fCrouchedWalkSpeed"),
                SendProp::float("m_fDuckSpeed"),
                SendProp::float("m_fUnDuckSpeed"),
            ],
        ),
        t(
            "DT_LocalPlayerExclusive",
            vec![
                SendProp::table("m_Local", "DT_Local"),
                // reserve ammo by ammo-type id
                SendProp::table("m_iAmmo", "m_iAmmo"),
                int_s("m_nTickBase", 32),
                int_u("m_hGroundEntity", 23),
                int_u("m_hLastWeapon", 23),
                SendProp::float("m_flLaggedMovementValue"),
            ],
        ),
        t(
            "DT_BasePlayer",
            vec![
                SendProp::baseclass("DT_BaseCombatCharacter"),
                int_u("m_lifeState", 3),
                SendProp::float("m_flMaxspeed"),
                SendProp::float("m_iFOV"),
                SendProp::float("m_iDefaultFOV"),
                int_u("m_fFlags", 16),
                SendProp::int_array("m_hViewModel", 23, 3, true),
                SendProp::table("localdata", "DT_LocalPlayerExclusive"),
            ],
        ),
        // [факт] the HL2 player code refuses to sprint (and plays the deny sound) when
        // the suit power is below 10; it must be networked as full
        t("DT_HL2Local", vec![SendProp::float("m_flSuitPower"), int_u("m_hLadder", 23)]),
        t(
            "DT_FuncLadder",
            vec![
                SendProp::baseclass("DT_BaseEntity"),
                SendProp::vector("m_vecPlayerMountPositionTop"),
                SendProp::vector("m_vecPlayerMountPositionBottom"),
                SendProp::vector("m_vecLadderDir"),
                int_u("m_bFakeLadder", 1),
            ],
        ),
        t(
            "DT_HL2_Player",
            vec![
                SendProp::baseclass("DT_BasePlayer"),
                SendProp::table("m_HL2Local", "DT_HL2Local"),
                int_u("m_fIsSprinting", 1),
            ],
        ),
        t(
            "DT_HL2MP_Player",
            vec![SendProp::baseclass("DT_HL2_Player"), int_u("m_fIsWalking", 1)],
        ),
        t(
            "DT_GMOD_Player",
            vec![
                SendProp::baseclass("DT_HL2MP_Player"),
                int_u("m_bSprintEnabled", 1),
                int_u("m_bCanWalk", 1),
                int_u("m_bCanZoom", 1),
                // [факт] reference server.dll: `m_PlayerClass`, 15-bit signed int — the
                // player_manager class id (util.NetworkStringToID); without it the client
                // runs no player-class hooks (sandbox jump boost) while predicting
                int_s("m_PlayerClass", 15),
                SendProp::float("m_StepSize"),
                SendProp::float("m_JumpPower"),
                SendProp::vector("m_ViewOffset"),
                SendProp::vector("m_ViewOffsetDucked"),
                // the gmod_hands entity drawn with the view model
                int_u("m_Hands", 23),
                SendProp::vector("m_PlayerColor"),
                SendProp::vector("m_WeaponColor"),
            ],
        ),
    ];
    let mut tables = tables;
    let slots = || element_names()[..DT_SLOTS].iter();
    tables.push(t("m_iAmmo", element_names()[..AMMO_SLOTS].iter().map(|n| int_u(n, 16)).collect()));
    tables.push(t("m_GMOD_bool", slots().map(|n| int_u(n, 1)).collect()));
    tables.push(t("m_GMOD_float", slots().map(|n| SendProp::float(n)).collect()));
    tables.push(t("m_GMOD_int", slots().map(|n| int_s(n, 32)).collect()));
    tables.push(t("m_GMOD_Vector", slots().map(|n| SendProp::vector(n)).collect()));
    tables.push(t("m_GMOD_QAngle", slots().map(|n| SendProp::vector(n)).collect()));
    tables.push(t("m_GMOD_EHANDLE", slots().map(|n| int_u(n, 23)).collect()));
    tables.push(t(
        "DT_PlayerResource",
        PLAYER_RESOURCE_ARRAYS
            .iter()
            .map(|(name, _, _)| SendProp::table(name, name))
            .collect(),
    ));
    for (name, bits, unsigned) in PLAYER_RESOURCE_ARRAYS {
        tables.push(t(
            name,
            element_names()
                .iter()
                .map(|e| SendProp::int(e, *bits, *unsigned))
                .collect(),
        ));
    }
    let classes = vec![
        ServerClass {
            name: "CWorld",
            table: "DT_WORLD",
        },
        ServerClass {
            name: "CGMOD_Player",
            table: "DT_GMOD_Player",
        },
        ServerClass {
            name: "CPlayerResource",
            table: "DT_PlayerResource",
        },
        ServerClass {
            name: "CPhysicsProp",
            table: "DT_PhysicsProp",
        },
        ServerClass {
            name: "CPredictedViewModel",
            table: "DT_PredictedViewModel",
        },
    ];
    let mut classes = classes;
    for (_, name, table) in WEAPON_CLASSES {
        classes.push(ServerClass { name, table });
    }
    classes.push(ServerClass {
        name: "CDynamicProp",
        table: "DT_DynamicProp",
    });
    classes.push(ServerClass {
        name: "CSENT_anim",
        table: "DT_SENT_anim",
    });
    classes.push(ServerClass {
        name: "CRopeKeyframe",
        table: "DT_RopeKeyframe",
    });
    classes.push(ServerClass {
        name: "CBaseEntity",
        table: "DT_BaseEntity",
    });
    classes.push(ServerClass {
        name: "CFuncLadder",
        table: "DT_FuncLadder",
    });
    classes.push(ServerClass {
        name: "CPhysBeam",
        table: "DT_PhysBeam",
    });
    (classes, TableSet { tables })
}

/// Classes, tables and flattened property lists, built once per server.
#[derive(Debug, Clone)]
pub struct NetClasses {
    pub classes: Vec<ServerClass>,
    pub tables: TableSet,
    pub flat: Vec<Vec<FlatProp>>,
}

impl NetClasses {
    pub fn new() -> Result<Self> {
        let (classes, tables) = tables();
        let flat = classes
            .iter()
            .map(|c| sendtable::flatten(&tables, c.table))
            .collect::<Result<_>>()?;
        Ok(Self {
            classes,
            tables,
            flat,
        })
    }

    /// Appends `svc_SendTable` for every table (class tables first, flagged as
    /// needing a decoder, then the rest) followed by `svc_ClassInfo`.
    pub fn write_signon(&self, w: &mut BitWriter) -> Result<()> {
        let leaf: Vec<&str> = self.classes.iter().map(|c| c.table).collect();
        for name in &leaf {
            sendtable::write_send_table(w, self.tables.get(name)?, true)?;
        }
        for t in &self.tables.tables {
            if !leaf.contains(&t.name) {
                sendtable::write_send_table(w, t, false)?;
            }
        }
        sendtable::write_class_info(w, &self.classes)
    }

    /// A new entity of `class` with every networked property at its default.
    pub fn new_entity(&self, index: u32, class: u32, serial: u32) -> EntityState {
        let props = self.flat[class as usize]
            .iter()
            .map(|p| PropValue::default_for(&p.kind))
            .collect();
        EntityState {
            index,
            class,
            serial,
            props,
        }
    }

    fn index_of(&self, class: u32, table: &str, name: &str) -> Option<usize> {
        self.flat[class as usize]
            .iter()
            .position(|p| p.table == table && p.name == name)
    }
}

/// Full networked state of one entity (values in flattened order).
#[derive(Debug, Clone, PartialEq)]
pub struct EntityState {
    pub index: u32,
    pub class: u32,
    pub serial: u32,
    pub props: Vec<PropValue>,
}

impl EntityState {
    /// `m_nModelIndex`, when the class networks it.
    pub fn model_index(&self, nc: &NetClasses) -> Option<i32> {
        let i = nc.index_of(self.class, "DT_BaseEntity", "m_nModelIndex")?;
        match self.props.get(i)? {
            PropValue::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// A rope's (material model index, start handle, end handle).
    pub fn rope_refs(&self, nc: &NetClasses) -> Option<(i32, i32, i32)> {
        if self.class != CLASS_ROPE {
            return None;
        }
        let get = |name: &str| match nc.index_of(self.class, "DT_RopeKeyframe", name).and_then(|i| self.props.get(i)) {
            Some(PropValue::Int(v)) => Some(*v),
            _ => None,
        };
        Some((get("m_iRopeMaterialModelIndex")?, get("m_hStartPoint")?, get("m_hEndPoint")?))
    }

    /// Sets a property by table and name. Panics on an unknown property: the set of
    /// properties is fixed at compile time by [`tables`], so this is a programming
    /// error caught by the unit tests.
    pub fn set(&mut self, nc: &NetClasses, table: &str, name: &str, v: PropValue) -> &mut Self {
        let i = nc
            .index_of(self.class, table, name)
            .unwrap_or_else(|| panic!("no networked prop {table}.{name}"));
        self.props[i] = v;
        self
    }
}

/// Everything the server networks at one tick, sorted by entity index.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub tick: i32,
    pub entities: Vec<EntityState>,
}

/// Encoded `svc_PacketEntities` payload.
#[derive(Debug)]
pub struct EncodedEntities {
    pub data: BitWriter,
    pub updated: u32,
    pub max_entries: u32,
}

/// Encodes `to` as a full update (`from == None`, every entity enters with all its
/// properties) or as a delta against a snapshot the client has acknowledged.
pub fn encode(nc: &NetClasses, to: &Snapshot, from: Option<&Snapshot>) -> Result<EncodedEntities> {
    let mut updates = Vec::new();
    let empty = Vec::new();
    let old = from.map(|f| &f.entities).unwrap_or(&empty);
    let (mut i, mut j) = (0, 0);
    loop {
        let n = to.entities.get(i);
        let o = old.get(j);
        match (n, o) {
            (None, None) => break,
            (Some(n), Some(o)) if n.index == o.index => {
                if n.class != o.class || n.serial != o.serial {
                    updates.push(enter(n));
                } else {
                    let props: Vec<_> = n
                        .props
                        .iter()
                        .zip(&o.props)
                        .enumerate()
                        .filter(|(_, (a, b))| a != b)
                        .map(|(k, (a, _))| (k, a))
                        .collect();
                    if !props.is_empty() {
                        updates.push(EntityUpdate::Delta {
                            index: n.index,
                            props,
                        });
                    }
                }
                i += 1;
                j += 1;
            }
            (Some(n), o) if o.map_or(true, |o| n.index < o.index) => {
                updates.push(enter(n));
                i += 1;
            }
            (_, Some(o)) => {
                updates.push(EntityUpdate::Leave {
                    index: o.index,
                    delete: true,
                });
                j += 1;
            }
            (Some(_), None) => unreachable!(),
        }
    }
    let mut data = BitWriter::with_capacity(16 * 1024);
    let flat_of = |c: u32| nc.flat.get(c as usize).map(|v| v.as_slice());
    let class_of = |idx: u32| to.entities.iter().find(|e| e.index == idx).map(|e| e.class);
    sendtable::write_entity_updates(
        &mut data,
        &updates,
        nc.classes.len(),
        from.is_some(),
        flat_of,
        class_of,
    )?;
    let max_entries = to.entities.last().map_or(0, |e| e.index + 1);
    Ok(EncodedEntities {
        data,
        updated: updates.len() as u32,
        max_entries,
    })
}

/// Enter-PVS update relative to the (empty) class baseline: only properties that
/// differ from the zero value are sent. Every property the server cares about is
/// set explicitly by the entity builders, so client-side constructor defaults are
/// only relied on for values the server leaves at zero.
fn enter(e: &EntityState) -> EntityUpdate<'_> {
    EntityUpdate::Enter {
        index: e.index,
        class_id: e.class,
        serial: e.serial,
        props: e
            .props
            .iter()
            .enumerate()
            .filter(|(_, v)| !v.is_zero())
            .collect(),
    }
}

/// Entity index of the player resource (after all player slots).
pub fn player_resource_index(max_clients: u8) -> u32 {
    max_clients as u32 + 1
}

/// Per-player row of the player resource.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceRow {
    pub slot: u8,
    pub ping: i32,
    pub score: i32,
    pub deaths: i32,
    pub team: i32,
    pub alive: bool,
    pub health: i32,
}

/// The player resource entity: scoreboard data (`Player:Frags()`, `:Deaths()`,
/// `:Ping()`, …) for every connected player.
pub fn player_resource(nc: &NetClasses, max_clients: u8, rows: &[ResourceRow]) -> EntityState {
    let mut e = nc.new_entity(player_resource_index(max_clients), CLASS_PLAYER_RESOURCE, 1);
    let names = element_names();
    for r in rows {
        let Some(el) = names.get(r.slot as usize + 1) else {
            continue;
        };
        let vals = [
            ("m_iPing", r.ping),
            ("m_iScore", r.score),
            ("m_iDeaths", r.deaths),
            ("m_bConnected", 1),
            ("m_iTeam", r.team),
            ("m_bAlive", r.alive as i32),
            ("m_iHealth", r.health),
        ];
        for (table, v) in vals {
            e.set(nc, table, el, PropValue::Int(v));
        }
    }
    e
}

/// Serial number used for the world entity (index 0).
pub const WORLD_SERIAL: u32 = 0;

/// The world entity (index 0) for `map`.
pub fn world(nc: &NetClasses, map: &MapInfo) -> EntityState {
    let mut e = nc.new_entity(0, CLASS_WORLD, WORLD_SERIAL);
    e.set(nc, "DT_BaseEntity", "m_nModelIndex", PropValue::Int(1))
        .set(
            nc,
            "DT_WORLD",
            "m_WorldMins",
            PropValue::Vector(map.world_mins),
        )
        .set(
            nc,
            "DT_WORLD",
            "m_WorldMaxs",
            PropValue::Vector(map.world_maxs),
        );
    e
}

/// A model entity (prop) as networked.
#[derive(Debug, Clone, PartialEq)]
pub struct PropState {
    /// `moveparent` handle (INVALID_EHANDLE when none).
    pub parent: i32,
    pub index: u32,
    pub serial: u32,
    pub model_index: i32,
    pub origin: [f32; 3],
    pub angles: [f32; 3],
    pub velocity: [f32; 3],
    pub skin: i32,
    pub body: i32,
    /// r, g, b, a
    pub color: [u8; 4],
    pub render_mode: i32,
    pub render_fx: i32,
    pub effects: i32,
    pub collision_group: i32,
    pub solid: i32,
    pub mins: [f32; 3],
    pub maxs: [f32; 3],
    pub move_type: i32,
    pub owner: i32,
    pub sim_tick: i32,
    pub awake: bool,
    /// Animation sequence (0 = the model's first) and playback rate.
    pub sequence: i32,
    pub playback_rate: f32,
}

/// The networked entity of a physics prop.
pub fn physics_prop(nc: &NetClasses, p: &PropState) -> EntityState {
    model_entity(nc, CLASS_PHYSICS_PROP, p, "")
}

/// A model entity of `class` (physics prop, dynamic prop or scripted "anim" entity
/// with `script` as its Lua class).
pub fn model_entity(nc: &NetClasses, class: u32, p: &PropState, script: &str) -> EntityState {
    let mut e = nc.new_entity(p.index, class, p.serial);
    let color = p.color[0] as i32
        | (p.color[1] as i32) << 8
        | (p.color[2] as i32) << 16
        | (p.color[3] as i32) << 24;
    e.set(
        nc,
        "DT_BaseEntity",
        "m_flSimulationTime",
        PropValue::Int(simulation_time_value(p.sim_tick, p.index)),
    )
    .set(nc, "DT_BaseEntity", "m_vecOrigin", PropValue::Vector(p.origin))
    .set(nc, "DT_BaseEntity", "m_nModelIndex", PropValue::Int(p.model_index))
    .set(nc, "DT_BaseEntity", "m_angRotation", PropValue::Vector(p.angles))
    .set(nc, "DT_BaseEntity", "movetype", PropValue::Int(p.move_type))
    .set(nc, "DT_BaseEntity", "m_CollisionGroup", PropValue::Int(p.collision_group))
    .set(nc, "DT_BaseEntity", "m_vecVelocity[0]", PropValue::Float(p.velocity[0]))
    .set(nc, "DT_BaseEntity", "m_vecVelocity[1]", PropValue::Float(p.velocity[1]))
    .set(nc, "DT_BaseEntity", "m_vecVelocity[2]", PropValue::Float(p.velocity[2]))
    .set(nc, "DT_BaseEntity", "m_nRenderFX", PropValue::Int(p.render_fx))
    .set(nc, "DT_BaseEntity", "m_nRenderMode", PropValue::Int(p.render_mode))
    .set(nc, "DT_BaseEntity", "m_fEffects", PropValue::Int(p.effects))
    .set(nc, "DT_BaseEntity", "m_clrRender", PropValue::Int(color))
    .set(nc, "DT_BaseEntity", "m_hOwnerEntity", PropValue::Int(p.owner))
    .set(nc, "DT_BaseEntity", "moveparent", PropValue::Int(p.parent))
    .set(nc, "DT_CollisionProperty", "m_vecMinsPreScaled", PropValue::Vector(p.mins))
    .set(nc, "DT_CollisionProperty", "m_vecMaxsPreScaled", PropValue::Vector(p.maxs))
    .set(nc, "DT_CollisionProperty", "m_vecMins", PropValue::Vector(p.mins))
    .set(nc, "DT_CollisionProperty", "m_vecMaxs", PropValue::Vector(p.maxs))
    .set(nc, "DT_CollisionProperty", "m_nSolidType", PropValue::Int(p.solid))
    .set(nc, "DT_BaseAnimating", "m_nSkin", PropValue::Int(p.skin))
    .set(nc, "DT_BaseAnimating", "m_nBody", PropValue::Int(p.body))
    .set(nc, "DT_BaseAnimating", "m_nSequence", PropValue::Int(p.sequence.clamp(0, 4095)));
    if p.sequence != 0 {
        // the client plays the sequence itself
        e.set(nc, "DT_BaseAnimating", "m_bClientSideAnimation", PropValue::Int(1))
            .set(nc, "DT_BaseAnimating", "m_flPlaybackRate", PropValue::Float(p.playback_rate));
    }
    if class == CLASS_PHYSICS_PROP {
        e.set(nc, "DT_PhysicsProp", "m_bAwake", PropValue::Int(p.awake as i32));
    }
    if class == CLASS_SENT_ANIM {
        e.set(nc, "DT_ScriptedEntity", "m_strScriptName", PropValue::String(script.to_string()));
    }
    e
}

/// A map brush entity as networked.
#[derive(Debug, Clone, PartialEq)]
pub struct BrushState {
    pub index: u32,
    pub serial: u32,
    pub model_index: i32,
    pub origin: [f32; 3],
    pub angles: [f32; 3],
    pub color: [u8; 4],
    pub render_mode: i32,
    pub render_fx: i32,
    pub effects: i32,
    pub solid: bool,
}

pub fn brush_entity(nc: &NetClasses, b: &BrushState) -> EntityState {
    let mut e = nc.new_entity(b.index, CLASS_BRUSH, b.serial);
    let [r, g, bl, a] = b.color;
    e.set(nc, "DT_BaseEntity", "m_nModelIndex", PropValue::Int(b.model_index))
        .set(nc, "DT_BaseEntity", "m_vecOrigin", PropValue::Vector(b.origin))
        .set(nc, "DT_BaseEntity", "m_angRotation", PropValue::Vector(b.angles))
        .set(nc, "DT_BaseEntity", "movetype", PropValue::Int(7)) // MOVETYPE_PUSH
        .set(nc, "DT_BaseEntity", "m_nRenderMode", PropValue::Int(b.render_mode))
        .set(nc, "DT_BaseEntity", "m_nRenderFX", PropValue::Int(b.render_fx))
        .set(nc, "DT_BaseEntity", "m_fEffects", PropValue::Int(b.effects))
        .set(nc, "DT_BaseEntity", "m_clrRender", PropValue::Int(i32::from_le_bytes([r, g, bl, a])))
        .set(nc, "DT_BaseEntity", "m_hOwnerEntity", PropValue::Int(INVALID_EHANDLE))
        .set(nc, "DT_BaseEntity", "moveparent", PropValue::Int(INVALID_EHANDLE))
        // SOLID_BSP / SOLID_NONE
        .set(nc, "DT_CollisionProperty", "m_nSolidType", PropValue::Int(if b.solid { 1 } else { 0 }));
    set_dtvars(nc, &mut e, &[]);
    e
}

/// A useable ladder: mount points relative to its origin ([факт] reference
/// server.dll: the ends are the entity origin plus stored offsets).
pub fn ladder_entity(nc: &NetClasses, index: u32, serial: u32, origin: [f32; 3], bottom: [f32; 3], top: [f32; 3]) -> EntityState {
    let mut e = nc.new_entity(index, CLASS_LADDER, serial);
    let rel = |v: [f32; 3]| [v[0] - origin[0], v[1] - origin[1], v[2] - origin[2]];
    e.set(nc, "DT_BaseEntity", "m_vecOrigin", PropValue::Vector(origin))
        .set(nc, "DT_BaseEntity", "m_fEffects", PropValue::Int(0x20))
        .set(nc, "DT_BaseEntity", "m_hOwnerEntity", PropValue::Int(INVALID_EHANDLE))
        .set(nc, "DT_BaseEntity", "moveparent", PropValue::Int(INVALID_EHANDLE))
        .set(nc, "DT_FuncLadder", "m_vecPlayerMountPositionTop", PropValue::Vector(rel(top)))
        .set(nc, "DT_FuncLadder", "m_vecPlayerMountPositionBottom", PropValue::Vector(rel(bottom)))
        .set(nc, "DT_FuncLadder", "m_vecLadderDir", PropValue::Vector([top[0] - bottom[0], top[1] - bottom[1], top[2] - bottom[2]]));
    set_dtvars(nc, &mut e, &[]);
    e
}

/// The physgun beam: on while the trigger is held, to the grabbed entity (hold point
/// in its space) or to where the beam hits ([гипотеза] meaning of `m_HoldPos`).
#[allow(clippy::too_many_arguments)]
pub fn phys_beam(nc: &NetClasses, index: u32, serial: u32, origin: [f32; 3], weapon: i32, on: bool, target: i32, hold: [f32; 3]) -> EntityState {
    let mut e = nc.new_entity(index, CLASS_PHYSBEAM, serial);
    e.set(nc, "DT_BaseEntity", "m_vecOrigin", PropValue::Vector(origin))
        .set(nc, "DT_BaseEntity", "m_hOwnerEntity", PropValue::Int(weapon))
        .set(nc, "DT_BaseEntity", "moveparent", PropValue::Int(INVALID_EHANDLE))
        .set(nc, "DT_PhysBeam", "m_hTargetEnt", PropValue::Int(target))
        .set(nc, "DT_PhysBeam", "m_HoldPos", PropValue::Vector(hold))
        .set(nc, "DT_PhysBeam", "m_bIsOn", PropValue::Int(on as i32));
    set_dtvars(nc, &mut e, &[]);
    e
}

/// A visual rope between two entities (offsets in each entity's local space).
#[derive(Debug, Clone, PartialEq)]
pub struct RopeState {
    pub index: u32,
    pub serial: u32,
    pub origin: [f32; 3],
    pub start: i32,
    pub end: i32,
    pub start_offset: [f32; 3],
    pub end_offset: [f32; 3],
    pub length: i32,
    pub width: f32,
    pub material: i32,
    pub flags: i32,
    pub texture_scale: f32,
}

pub fn rope(nc: &NetClasses, r: &RopeState) -> EntityState {
    let mut e = nc.new_entity(r.index, CLASS_ROPE, r.serial);
    e.set(nc, "DT_RopeKeyframe", "m_hStartPoint", PropValue::Int(r.start))
        .set(nc, "DT_RopeKeyframe", "m_hEndPoint", PropValue::Int(r.end))
        .set(nc, "DT_RopeKeyframe", "m_vecStartOffset", PropValue::Vector(r.start_offset))
        .set(nc, "DT_RopeKeyframe", "m_vecEndOffset", PropValue::Vector(r.end_offset))
        .set(nc, "DT_RopeKeyframe", "m_clrRender", PropValue::Int(-1))
        .set(nc, "DT_RopeKeyframe", "m_RopeLength", PropValue::Int(r.length.clamp(0, 32767)))
        // both ends attached
        .set(nc, "DT_RopeKeyframe", "m_fLockedPoints", PropValue::Int(3))
        .set(nc, "DT_RopeKeyframe", "m_RopeFlags", PropValue::Int(r.flags))
        .set(nc, "DT_RopeKeyframe", "m_nSegments", PropValue::Int(8))
        .set(nc, "DT_RopeKeyframe", "m_iRopeMaterialModelIndex", PropValue::Int(r.material))
        .set(nc, "DT_RopeKeyframe", "m_Subdiv", PropValue::Int(2))
        .set(nc, "DT_RopeKeyframe", "m_TextureScale", PropValue::Float(r.texture_scale))
        .set(nc, "DT_RopeKeyframe", "m_Width", PropValue::Float(r.width))
        .set(nc, "DT_RopeKeyframe", "m_vecOrigin", PropValue::Vector(r.origin));
    e
}

/// Server-side state of a player pawn that is networked.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerPawn {
    /// Position, velocity, ground and crouch state.
    pub mv: crate::movement::MoveState,
    pub angles: [f32; 3],
    pub tick_base: i32,
    pub sim_tick: i32,
    pub move_type: i32,
    pub health: i32,
    pub model_index: i32,
    /// `m_iDefaultFOV` (the client's `fov_desired` when known).
    pub default_fov: f32,
    /// Movement settings of this player (speeds and jump power can be changed by Lua).
    pub params: crate::movement::MoveParams,
    /// Networked handles of the player's weapons and view model.
    pub inventory: Inventory,
    /// Flashlight on (`EF_DIMLIGHT`).
    pub flashlight: bool,
    /// `gmod_hands` entity handle (`m_Hands`).
    pub hands: i32,
    /// The player's NetworkVars (player classes use them).
    pub dt: Vec<DtVar>,
    /// Reserve ammo by ammo-type id.
    pub ammo: Vec<i32>,
    pub player_color: [f32; 3],
    pub weapon_color: [f32; 3],
    /// `m_flNextAttack`: no weapon frame before this time (deploy, reload).
    pub next_attack: f64,
    /// `m_hLastWeapon`: the weapon held before the current one.
    pub last_weapon: i32,
    /// `m_PlayerClass`: player_manager class id.
    pub player_class: i32,
}

/// Entity handles a player networks for its weapons.
#[derive(Debug, Clone, PartialEq)]
pub struct Inventory {
    pub active: i32,
    pub weapons: Vec<i32>,
    pub view_model: i32,
    /// Networked `m_hMyWeapons` slots: 0 until first used ([факт] 0 in unused slots
    /// is what clients joined with fine; `INVALID_EHANDLE` in all 48 slots at join
    /// coincided with a client crash), then the last weapon held there.
    pub slots: Vec<i32>,
}

impl Default for Inventory {
    fn default() -> Self {
        Self {
            active: INVALID_EHANDLE,
            weapons: Vec::new(),
            view_model: INVALID_EHANDLE,
            slots: vec![0; MAX_WEAPONS],
        }
    }
}

/// A weapon entity as networked.
#[derive(Debug, Clone, PartialEq)]
pub struct WeaponState {
    pub index: u32,
    pub serial: u32,
    pub class: u32,
    pub origin: [f32; 3],
    pub angles: [f32; 3],
    pub world_model: i32,
    pub view_model: i32,
    pub state: i32,
    pub owner: i32,
    pub clip1: i32,
    pub clip2: i32,
    /// Ammo-type ids (-1 = none).
    pub ammo1: i32,
    pub ammo2: i32,
    /// Next attack times and idle time (`DT_LocalActiveWeaponData`).
    pub next_primary: f64,
    pub next_secondary: f64,
    pub idle_time: f64,
    pub sim_tick: i32,
    /// `CWeaponSWEP`: the Lua class and hold type.
    pub script_name: String,
    pub hold_type: String,
    /// Physics gun: grabbed entity handle and hit position on it (local space).
    pub grabbed: i32,
    pub hit_local: [f32; 3],
    /// Physics gun: its beam entity.
    pub beam: i32,
    /// Gravity gun: effect state and claws open.
    pub cannon_effect: i32,
    pub cannon_open: bool,
    /// Gravity gun: the held object.
    pub cannon_held: i32,
}

pub fn weapon(nc: &NetClasses, w: &WeaponState) -> EntityState {
    let mut e = nc.new_entity(w.index, w.class, w.serial);
    let active = w.state == WEAPON_IS_ACTIVE;
    let effects = if active {
        EF_BONEMERGE | EF_BONEMERGE_FASTCULL | EF_NOSHADOW
    } else if w.state == WEAPON_IS_CARRIED {
        EF_NODRAW | EF_NOSHADOW
    } else {
        0
    };
    e.set(nc, "DT_BaseEntity", "m_flSimulationTime", PropValue::Int(simulation_time_value(w.sim_tick, w.index)))
        .set(nc, "DT_BaseEntity", "m_vecOrigin", PropValue::Vector(w.origin))
        .set(nc, "DT_BaseEntity", "m_angRotation", PropValue::Vector(w.angles))
        // [факт] live client: a carried weapon's own model must be the view model —
        // the client picks the weapon's animation sequences (and their durations)
        // from it; with the world model it idled every tick
        .set(nc, "DT_BaseEntity", "m_nModelIndex", PropValue::Int(if w.state == WEAPON_NOT_CARRIED { w.world_model } else { w.view_model }))
        .set(nc, "DT_BaseEntity", "m_fEffects", PropValue::Int(effects))
        .set(nc, "DT_BaseEntity", "m_clrRender", PropValue::Int(-1))
        .set(nc, "DT_BaseEntity", "m_hOwnerEntity", PropValue::Int(w.owner))
        .set(nc, "DT_BaseEntity", "moveparent", PropValue::Int(if w.state == WEAPON_NOT_CARRIED { INVALID_EHANDLE } else { w.owner }))
        .set(nc, "DT_BaseCombatWeapon", "m_iViewModelIndex", PropValue::Int(w.view_model))
        .set(nc, "DT_BaseCombatWeapon", "m_iWorldModelIndex", PropValue::Int(w.world_model))
        .set(nc, "DT_BaseCombatWeapon", "m_iState", PropValue::Int(w.state))
        .set(nc, "DT_BaseCombatWeapon", "m_hOwner", PropValue::Int(w.owner))
        .set(nc, "DT_LocalActiveWeaponData", "m_flNextPrimaryAttack", PropValue::Double(w.next_primary))
        .set(nc, "DT_LocalActiveWeaponData", "m_flNextSecondaryAttack", PropValue::Double(w.next_secondary))
        .set(nc, "DT_LocalActiveWeaponData", "m_flTimeWeaponIdle", PropValue::Double(w.idle_time))
        .set(nc, "DT_LocalWeaponData", "m_iPrimaryAmmoType", PropValue::Int(w.ammo1))
        .set(nc, "DT_LocalWeaponData", "m_iSecondaryAmmoType", PropValue::Int(w.ammo2))
        .set(nc, "DT_LocalWeaponData", "m_iClip1", PropValue::Int(w.clip1))
        .set(nc, "DT_LocalWeaponData", "m_iClip2", PropValue::Int(w.clip2));
    let table = WEAPON_CLASSES[(w.class - CLASS_WEAPON_FIRST) as usize].2;
    if table == "DT_WeaponSWEP" {
        e.set(nc, "DT_ScriptedEntity", "m_strScriptName", PropValue::String(w.script_name.clone()))
            .set(nc, "DT_WeaponSWEP", "m_strHoldType", PropValue::String(w.hold_type.clone()));
    }
    // [факт] live client: the gravity gun idles in effect state 2 (it predicted 2
    // where the server sent nothing)
    if table == "DT_WeaponPhysCannon" {
        e.set(nc, "DT_WeaponPhysCannon", "m_EffectState", PropValue::Int(w.cannon_effect))
            .set(nc, "DT_WeaponPhysCannon", "m_bOpen", PropValue::Int(w.cannon_open as i32))
            .set(nc, "DT_WeaponPhysCannon", "m_hAttachedObject", PropValue::Int(w.cannon_held))
            .set(nc, "DT_WeaponPhysCannon", "m_bActive", PropValue::Int((w.cannon_held != INVALID_EHANDLE) as i32));
    }
    if table == "DT_WeaponPhysGun" {
        e.set(nc, "DT_WeaponPhysGun", "m_hPhysBeam", PropValue::Int(w.beam))
            .set(nc, "DT_WeaponPhysGun", "m_hGrabbedEntity", PropValue::Int(w.grabbed))
            .set(nc, "DT_WeaponPhysGun", "m_vHitPosLocal", PropValue::Vector(w.hit_local));
    }
    e
}

/// A player's view model as networked.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewModelState {
    pub index: u32,
    pub serial: u32,
    pub model: i32,
    pub weapon: i32,
    pub owner: i32,
    pub sequence: i32,
    pub parity: i32,
    pub hidden: bool,
    /// Playback rate of the sequence (a draw plays at the deploy speed).
    pub rate: f32,
    pub muzzle_flash: i32,
}

pub fn view_model(nc: &NetClasses, v: &ViewModelState) -> EntityState {
    let mut e = nc.new_entity(v.index, CLASS_VIEWMODEL, v.serial);
    e.set(nc, "DT_BaseViewModel", "m_nModelIndex", PropValue::Int(v.model))
        .set(nc, "DT_BaseViewModel", "m_hWeapon", PropValue::Int(v.weapon))
        .set(nc, "DT_BaseViewModel", "m_hOwner", PropValue::Int(v.owner))
        .set(nc, "DT_BaseViewModel", "m_nSequence", PropValue::Int(v.sequence))
        .set(nc, "DT_BaseViewModel", "m_nNewSequenceParity", PropValue::Int(v.parity))
        .set(nc, "DT_BaseViewModel", "m_nAnimationParity", PropValue::Int(v.parity))
        .set(nc, "DT_BaseViewModel", "m_fEffects", PropValue::Int(if v.hidden { EF_NODRAW } else { 0 }))
        .set(nc, "DT_BaseViewModel", "m_flPlaybackRate", PropValue::Float(v.rate))
        .set(nc, "DT_BaseViewModel", "m_nMuzzleFlashParity", PropValue::Int(v.muzzle_flash & 3));
    e
}

/// Standing eye height of a player above its origin.
pub const VIEW_HEIGHT: f32 = 64.0;
pub const VIEW_HEIGHT_DUCKED: f32 = 28.0;
pub const STEP_SIZE: f32 = 18.0;
pub const JUMP_POWER: f32 = 200.0;
pub const MAX_SPEED: f32 = 400.0;

/// The networked entity of the player in `slot` (entity index `slot + 1`).
pub fn player(
    nc: &NetClasses,
    slot: u8,
    serial: u32,
    p: &PlayerPawn,
    mp: &crate::movement::MoveParams,
) -> EntityState {
    let index = slot as u32 + 1;
    let mut e = nc.new_entity(index, CLASS_PLAYER, serial);
    let flags = FL_CLIENT | p.mv.duck_flags() | if p.mv.on_ground { FL_ONGROUND } else { 0 };
    let (hull_min, hull_max) = p.mv.hull();
    let ground = if p.mv.on_ground {
        ehandle(0, WORLD_SERIAL)
    } else {
        INVALID_EHANDLE
    };
    e.set(
        nc,
        "DT_BaseEntity",
        "m_flSimulationTime",
        PropValue::Int(simulation_time_value(p.sim_tick, index)),
    )
    .set(
        nc,
        "DT_BaseEntity",
        "m_vecOrigin",
        PropValue::Vector(p.mv.origin),
    )
    .set(
        nc,
        "DT_BaseEntity",
        "m_nModelIndex",
        PropValue::Int(p.model_index),
    )
    .set(
        nc,
        "DT_BaseEntity",
        "m_angRotation",
        PropValue::Vector([0.0, p.angles[1], 0.0]),
    )
    .set(nc, "DT_BaseEntity", "movetype", PropValue::Int(p.move_type))
    .set(
        nc,
        "DT_BaseEntity",
        "m_vecVelocity[0]",
        PropValue::Float(p.mv.velocity[0]),
    )
    .set(
        nc,
        "DT_BaseEntity",
        "m_vecVelocity[1]",
        PropValue::Float(p.mv.velocity[1]),
    )
    .set(
        nc,
        "DT_BaseEntity",
        "m_vecVelocity[2]",
        PropValue::Float(p.mv.velocity[2]),
    )
    .set(
        nc,
        "DT_BaseEntity",
        "m_iTeamNum",
        PropValue::Int(TEAM_UNASSIGNED),
    )
    .set(nc, "DT_BaseEntity", "m_iHealth", PropValue::Int(p.health))
    .set(nc, "DT_BaseEntity", "m_iMaxHealth", PropValue::Int(100))
    .set(
        nc,
        "DT_BaseFlex",
        "m_vecViewOffset[2]",
        PropValue::Float(p.mv.view_z),
    )
    .set(
        nc,
        "DT_CollisionProperty",
        "m_vecMinsPreScaled",
        PropValue::Vector(hull_min),
    )
    .set(
        nc,
        "DT_CollisionProperty",
        "m_vecMaxsPreScaled",
        PropValue::Vector(hull_max),
    )
    .set(
        nc,
        "DT_CollisionProperty",
        "m_vecMins",
        PropValue::Vector(hull_min),
    )
    .set(
        nc,
        "DT_CollisionProperty",
        "m_vecMaxs",
        PropValue::Vector(hull_max),
    )
    // SOLID_BBOX
    .set(
        nc,
        "DT_CollisionProperty",
        "m_nSolidType",
        PropValue::Int(2),
    )
    .set(
        nc,
        "DT_Local",
        "m_vecPunchAngle",
        PropValue::Vector(p.mv.punch),
    )
    .set(
        nc,
        "DT_Local",
        "m_vecPunchAngleVel",
        PropValue::Vector(p.mv.punch_vel),
    )
    .set(
        nc,
        "DT_Local",
        "m_bDucked",
        PropValue::Int(p.mv.ducked as i32),
    )
    .set(
        nc,
        "DT_Local",
        "m_bDucking",
        PropValue::Int(p.mv.ducking as i32),
    )
    .set(
        nc,
        "DT_Local",
        "m_flDucktime",
        PropValue::Float(p.mv.duck_time),
    )
    .set(nc, "DT_Local", "m_bDrawViewmodel", PropValue::Int(1))
    .set(nc, "DT_Local", "m_bWearingSuit", PropValue::Int(1))
    .set(nc, "DT_Local", "m_flStepSize", PropValue::Float(STEP_SIZE))
    .set(nc, "DT_Local", "m_bAllowAutoMovement", PropValue::Int(1))
    .set(
        nc,
        "DT_Local",
        "m_flFallVelocity",
        PropValue::Float(p.mv.fall_velocity),
    )
    .set(
        nc,
        "DT_Local",
        "m_fSprintSpeed",
        PropValue::Float(mp.run_speed),
    )
    .set(
        nc,
        "DT_Local",
        "m_fWalkSpeed",
        PropValue::Float(mp.walk_speed),
    )
    .set(
        nc,
        "DT_Local",
        "m_fSlowWalkSpeed",
        PropValue::Float(mp.slow_walk_speed),
    )
    .set(nc, "DT_Local", "m_fLadderSpeed", PropValue::Float(200.0))
    .set(
        nc,
        "DT_Local",
        "m_fCrouchedWalkSpeed",
        PropValue::Float(mp.crouched_speed),
    )
    .set(
        nc,
        "DT_Local",
        "m_fDuckSpeed",
        PropValue::Float(mp.duck_time),
    )
    .set(
        nc,
        "DT_Local",
        "m_fUnDuckSpeed",
        PropValue::Float(mp.unduck_time),
    )
    .set(
        nc,
        "DT_LocalPlayerExclusive",
        "m_nTickBase",
        PropValue::Int(p.tick_base),
    )
    .set(
        nc,
        "DT_LocalPlayerExclusive",
        "m_hGroundEntity",
        PropValue::Int(ground),
    )
    .set(
        nc,
        "DT_LocalPlayerExclusive",
        "m_flLaggedMovementValue",
        PropValue::Float(1.0),
    )
    // LIFE_DEAD = 2
    .set(nc, "DT_BasePlayer", "m_lifeState", PropValue::Int(if p.health <= 0 { 2 } else { 0 }))
    .set(
        nc,
        "DT_BasePlayer",
        "m_flMaxspeed",
        PropValue::Float(p.mv.max_speed),
    )
    .set(
        nc,
        "DT_BasePlayer",
        "m_iDefaultFOV",
        PropValue::Float(p.default_fov),
    )
    .set(nc, "DT_BasePlayer", "m_fFlags", PropValue::Int(flags))
    // EF_DIMLIGHT = 4: the flashlight
    .set(nc, "DT_BaseEntity", "m_fEffects", PropValue::Int(if p.flashlight { 4 } else { 0 }))
    .set(
        nc,
        "DT_BasePlayer",
        "m_hViewModel",
        PropValue::IntArray(vec![p.inventory.view_model, INVALID_EHANDLE, INVALID_EHANDLE]),
    )
    .set(
        nc,
        "DT_BaseCombatCharacter",
        "m_hActiveWeapon",
        PropValue::Int(p.inventory.active),
    )
    // [факт] without these the client predicts sprinting as disabled (its maxspeed
    // stays at the walk speed while the server sprints → constant corrections)
    .set(nc, "DT_GMOD_Player", "m_bSprintEnabled", PropValue::Int(1))
    .set(nc, "DT_HL2Local", "m_flSuitPower", PropValue::Float(100.0))
    .set(nc, "DT_HL2Local", "m_hLadder", PropValue::Int(p.mv.ladder_handle))
    // [факт] the client predicts sprinting while IN_SPEED is held (noclip too)
    .set(nc, "DT_HL2_Player", "m_fIsSprinting", PropValue::Int(p.mv.sprinting as i32))
    .set(nc, "DT_HL2MP_Player", "m_fIsWalking", PropValue::Int(p.mv.walking as i32))
    .set(nc, "DT_GMOD_Player", "m_bCanWalk", PropValue::Int(1))
    .set(nc, "DT_GMOD_Player", "m_bCanZoom", PropValue::Int(1))
    .set(nc, "DT_GMOD_Player", "m_PlayerClass", PropValue::Int(p.player_class))
    .set(nc, "DT_BCCLocalPlayerExclusive", "m_flNextAttack", PropValue::Double(p.next_attack))
    .set(nc, "DT_LocalPlayerExclusive", "m_hLastWeapon", PropValue::Int(p.last_weapon))
    .set(
        nc,
        "DT_GMOD_Player",
        "m_StepSize",
        PropValue::Float(STEP_SIZE),
    )
    .set(
        nc,
        "DT_GMOD_Player",
        "m_JumpPower",
        PropValue::Float(mp.jump_power),
    )
    .set(
        nc,
        "DT_GMOD_Player",
        "m_ViewOffset",
        PropValue::Vector([0.0, 0.0, VIEW_HEIGHT]),
    )
    .set(
        nc,
        "DT_GMOD_Player",
        "m_ViewOffsetDucked",
        PropValue::Vector([0.0, 0.0, VIEW_HEIGHT_DUCKED]),
    )
    .set(nc, "DT_GMOD_Player", "m_Hands", PropValue::Int(p.hands))
    .set(nc, "DT_GMOD_Player", "m_PlayerColor", PropValue::Vector(p.player_color))
    .set(nc, "DT_GMOD_Player", "m_WeaponColor", PropValue::Vector(p.weapon_color))
    .set(nc, "DT_BaseAnimating", "m_bClientSideAnimation", PropValue::Int(1))
    .set(nc, "DT_BaseAnimating", "m_flPlaybackRate", PropValue::Float(1.0));
    set_dtvars(nc, &mut e, &p.dt);
    for (i, name) in element_names()[..AMMO_SLOTS].iter().enumerate() {
        e.set(nc, "m_iAmmo", name, PropValue::Int(p.ammo.get(i).copied().unwrap_or(0).clamp(0, 65535)));
    }
    // [факт] live client: slots emptied to 0 (= the world) on death froze the game,
    // last log line `m_hMyWeapons - EHandles differ (net) 0x… (pred) 0`
    for (i, name) in element_names()[..MAX_WEAPONS].iter().enumerate() {
        let w = p.inventory.weapons.get(i).copied().unwrap_or(p.inventory.slots[i]);
        e.set(nc, "m_hMyWeapons", name, PropValue::Int(w));
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use srcds_protocol::bitbuf::BitReader;

    fn map() -> MapInfo {
        MapInfo {
            name: "m".into(),
            md5: [0; 16],
            brush_models: 1,
            sky_name: "s".into(),
            world_mins: [-1.0; 3],
            world_maxs: [1.0; 3],
            spawn_points: Vec::new(),
                entities: Vec::new(),
        }
    }

    fn pawn() -> PlayerPawn {
        let mut mv = crate::movement::MoveState::new([1.0, 2.0, 3.0]);
        mv.on_ground = true;
        mv.max_speed = 200.0;
        PlayerPawn {
            mv,
            angles: [0.0, 90.0, 0.0],
            tick_base: 1000,
            sim_tick: 1000,
            move_type: MOVETYPE_WALK,
            health: 100,
            model_index: 2,
            default_fov: DEFAULT_FOV,
            params: Default::default(),
            inventory: Default::default(),
            flashlight: false,
            hands: INVALID_EHANDLE,
            dt: Vec::new(),
            ammo: Vec::new(),
            player_color: [0.24, 0.34, 0.41],
            weapon_color: [0.3, 1.8, 2.1],
            next_attack: 0.0,
            last_weapon: INVALID_EHANDLE,
            player_class: 0,
        }
    }

    #[test]
    fn tables_flatten_and_props_exist() {
        let nc = NetClasses::new().unwrap();
        // every prop set by the builders resolves (they panic otherwise)
        let _ = world(&nc, &map());
        let p = player(&nc, 0, 1, &pawn(), &Default::default());
        assert_eq!(p.index, 1);
        // the player's flattened list starts with its member sub-table props
        let names: Vec<_> = nc.flat[CLASS_PLAYER as usize]
            .iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names[0], "m_vecMinsPreScaled");
        assert!(names.contains(&"m_vecOrigin"));
        assert!(names.contains(&"m_nTickBase"));
        let mut w = BitWriter::with_capacity(64 * 1024);
        nc.write_signon(&mut w).unwrap();
        assert!(w.bit_len() > 1000);
    }

    #[test]
    fn simulation_time_matches_reference_pattern() {
        // reference traffic: player (index 1) at tick 2070 → 70
        assert_eq!(simulation_time_value(2070, 1), 70);
        // the base steps every 100 ticks, offset by index % 32
        assert_eq!(simulation_time_value(2100, 1), 100);
        assert_eq!(simulation_time_value(2101, 1), 1);
        assert_eq!(simulation_time_value(2101, 33), 1);
    }

    #[test]
    fn full_then_delta() {
        let nc = NetClasses::new().unwrap();
        let s1 = Snapshot {
            tick: 10,
            entities: vec![
                world(&nc, &map()),
                player(&nc, 0, 1, &pawn(), &Default::default()),
            ],
        };
        let full = encode(&nc, &s1, None).unwrap();
        assert_eq!(full.updated, 2);
        assert_eq!(full.max_entries, 2);
        // unchanged → nothing to send but the deletions terminator
        let none = encode(&nc, &s1, Some(&s1)).unwrap();
        assert_eq!(none.updated, 0);
        assert_eq!(none.data.bit_len(), 1);
        let mut p2 = pawn();
        p2.mv.origin[0] = 50.0;
        p2.tick_base += 1;
        let s2 = Snapshot {
            tick: 11,
            entities: vec![
                world(&nc, &map()),
                player(&nc, 0, 1, &p2, &Default::default()),
            ],
        };
        let d = encode(&nc, &s2, Some(&s1)).unwrap();
        assert_eq!(d.updated, 1);
        // header of entity 1: offset 1 (relative to entity -1 → 1), leave 0, enter 0
        let b = d.data.as_bytes().to_vec();
        let mut r = BitReader::new(&b);
        assert_eq!(r.read_ubits(2).unwrap(), 0);
        assert_eq!(r.read_ubits(4).unwrap(), 1);
        assert!(!r.read_bit().unwrap());
        assert!(!r.read_bit().unwrap());
        // removal of the player
        let s3 = Snapshot {
            tick: 12,
            entities: vec![world(&nc, &map())],
        };
        let gone = encode(&nc, &s3, Some(&s2)).unwrap();
        assert_eq!(gone.updated, 1);
        assert_eq!(gone.max_entries, 1);
    }

    /// Weapon slots: unused = 0 (as clients joined fine with), emptied = the last
    /// weapon's (now dead) handle, never the world.
    #[test]
    fn weapon_slots_never_become_the_world() {
        let nc = NetClasses::new().unwrap();
        let mut p = pawn();
        p.inventory.weapons = vec![ehandle(5, 1)];
        let e = player(&nc, 0, 1, &p, &Default::default());
        let i0 = nc.index_of(CLASS_PLAYER, "m_hMyWeapons", element_names()[0]).unwrap();
        let i1 = nc.index_of(CLASS_PLAYER, "m_hMyWeapons", element_names()[1]).unwrap();
        assert_eq!(e.props[i0], PropValue::Int(ehandle(5, 1)));
        assert_eq!(e.props[i1], PropValue::Int(0));
        // after death the slot keeps the removed weapon (never the world)
        p.inventory.slots[0] = ehandle(5, 1);
        p.inventory.weapons.clear();
        let e = player(&nc, 0, 1, &p, &Default::default());
        assert_eq!(e.props[i0], PropValue::Int(ehandle(5, 1)));
    }

    /// NetworkVars land in GMod's per-type slot tables; unused entity slots are "no
    /// entity"; players animate client-side and carry their hands entity.
    #[test]
    fn dtvars_and_player_hands_are_networked() {
        let nc = NetClasses::new().unwrap();
        let mut p = pawn();
        p.hands = ehandle(40, 3);
        p.dt = vec![(0, 2, PropValue::Int(1)), (5, 0, PropValue::Int(ehandle(7, 1))), (6, 1, PropValue::String("hi".into())), (3, 31, PropValue::Vector([1.0, 2.0, 3.0]))];
        let e = player(&nc, 0, 1, &p, &Default::default());
        let get = |t: &str, n: &str| e.props[nc.index_of(CLASS_PLAYER, t, n).unwrap()].clone();
        assert_eq!(get("m_GMOD_bool", "002"), PropValue::Int(1));
        assert_eq!(get("m_GMOD_EHANDLE", "000"), PropValue::Int(ehandle(7, 1)));
        assert_eq!(get("m_GMOD_EHANDLE", "001"), PropValue::Int(INVALID_EHANDLE));
        assert_eq!(get("DT_BaseEntity", "m_GMOD_String1"), PropValue::String("hi".into()));
        assert_eq!(get("m_GMOD_Vector", "031"), PropValue::Vector([1.0, 2.0, 3.0]));
        assert_eq!(get("DT_GMOD_Player", "m_Hands"), PropValue::Int(ehandle(40, 3)));
        assert_eq!(get("DT_BaseAnimating", "m_bClientSideAnimation"), PropValue::Int(1));
    }
}
//! `Vector` and `Angle` values (three single-precision floats, like the engine).
//!
//! Methods live in the shared metatables returned by `FindMetaTable("Vector")` /
//! `FindMetaTable("Angle")`, so the Lua extensions GMod ships (`extensions/vector.lua`)
//! and engine methods implemented here sit side by side.

use mlua::{FromLua, Lua, MetaMethod, Table, UserData, UserDataMethods, Value};

use crate::meta;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vector(pub [f32; 3]);

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Angle(pub [f32; 3]);

impl FromLua for Vector {
    fn from_lua(v: Value, _: &Lua) -> mlua::Result<Self> {
        match v {
            Value::UserData(ud) => Ok(*ud.borrow::<Vector>()?),
            other => Err(mlua::Error::FromLuaConversionError {
                from: other.type_name(),
                to: "Vector".into(),
                message: None,
            }),
        }
    }
}

impl FromLua for Angle {
    fn from_lua(v: Value, _: &Lua) -> mlua::Result<Self> {
        match v {
            Value::UserData(ud) => Ok(*ud.borrow::<Angle>()?),
            other => Err(mlua::Error::FromLuaConversionError {
                from: other.type_name(),
                to: "Angle".into(),
                message: None,
            }),
        }
    }
}

fn component(key: &Value, names: [&str; 3], alt: [&str; 3]) -> Option<usize> {
    match key {
        Value::String(s) => {
            let s = s.to_str().ok()?;
            (0..3).find(|&i| s == names[i] || s == alt[i])
        }
        Value::Integer(i) if (1..=3).contains(i) => Some(*i as usize - 1),
        Value::Number(n) if (1.0..=3.0).contains(n) && n.fract() == 0.0 => Some(*n as usize - 1),
        _ => None,
    }
}

fn num(v: &Value) -> Option<f32> {
    match v {
        Value::Integer(i) => Some(*i as f32),
        Value::Number(n) => Some(*n as f32),
        _ => None,
    }
}

fn fmt3(v: [f32; 3]) -> String {
    format!("{:.6} {:.6} {:.6}", v[0], v[1], v[2])
}

macro_rules! three_float_type {
    ($ty:ident, $meta:literal, $names:expr, $alt:expr) => {
        impl UserData for $ty {
            fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
                m.add_meta_method(MetaMethod::Index, |lua, this, key: Value| {
                    if let Some(i) = component(&key, $names, $alt) {
                        return Ok(Value::Number(this.0[i] as f64));
                    }
                    let t: Table = meta::get(lua, $meta)?;
                    t.raw_get::<Value>(key)
                });
                m.add_meta_method_mut(MetaMethod::NewIndex, |_, this, (key, v): (Value, Value)| {
                    if let (Some(i), Some(n)) = (component(&key, $names, $alt), num(&v)) {
                        this.0[i] = n;
                    }
                    Ok(())
                });
                m.add_meta_method(MetaMethod::ToString, |_, this, ()| Ok(fmt3(this.0)));
                m.add_meta_method(MetaMethod::Eq, |_, this, other: Value| {
                    Ok(match other {
                        Value::UserData(ud) => ud.borrow::<$ty>().map(|o| o.0 == this.0).unwrap_or(false),
                        _ => false,
                    })
                });
                m.add_meta_method(MetaMethod::Unm, |_, this, ()| {
                    Ok($ty([-this.0[0], -this.0[1], -this.0[2]]))
                });
                m.add_meta_function(MetaMethod::Add, |_, (a, b): ($ty, $ty)| {
                    Ok($ty([a.0[0] + b.0[0], a.0[1] + b.0[1], a.0[2] + b.0[2]]))
                });
                m.add_meta_function(MetaMethod::Sub, |_, (a, b): ($ty, $ty)| {
                    Ok($ty([a.0[0] - b.0[0], a.0[1] - b.0[1], a.0[2] - b.0[2]]))
                });
                m.add_meta_function(MetaMethod::Mul, |lua, (a, b): (Value, Value)| {
                    let (v, s) = match (&a, &b) {
                        (Value::UserData(_), x) if num(x).is_some() => ($ty::from_lua(a.clone(), lua)?, num(x).unwrap()),
                        (x, Value::UserData(_)) if num(x).is_some() => ($ty::from_lua(b.clone(), lua)?, num(x).unwrap()),
                        _ => {
                            // component-wise product
                            let (p, q) = ($ty::from_lua(a, lua)?, $ty::from_lua(b, lua)?);
                            return Ok($ty([p.0[0] * q.0[0], p.0[1] * q.0[1], p.0[2] * q.0[2]]));
                        }
                    };
                    Ok($ty([v.0[0] * s, v.0[1] * s, v.0[2] * s]))
                });
                m.add_meta_function(MetaMethod::Div, |lua, (a, b): (Value, Value)| {
                    let v = $ty::from_lua(a, lua)?;
                    match num(&b) {
                        Some(s) => Ok($ty([v.0[0] / s, v.0[1] / s, v.0[2] / s])),
                        None => {
                            let q = $ty::from_lua(b, lua)?;
                            Ok($ty([v.0[0] / q.0[0], v.0[1] / q.0[1], v.0[2] / q.0[2]]))
                        }
                    }
                });
            }
        }
    };
}

three_float_type!(Vector, "Vector", ["x", "y", "z"], ["x", "y", "z"]);
three_float_type!(Angle, "Angle", ["p", "y", "r"], ["pitch", "yaw", "roll"]);

fn len3(v: [f32; 3]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn args3(lua: &Lua, a: Value, b: Option<f32>, c: Option<f32>) -> mlua::Result<[f32; 3]> {
    match a {
        Value::Nil => Ok([0.0; 3]),
        Value::UserData(ud) => {
            if let Ok(v) = ud.borrow::<Vector>() {
                Ok(v.0)
            } else {
                Ok(ud.borrow::<Angle>()?.0)
            }
        }
        Value::String(s) => {
            let s = s.to_str()?.to_string();
            let mut it = s.split_whitespace().map(|t| t.parse::<f32>().unwrap_or(0.0));
            Ok([it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0)])
        }
        other => {
            let x = num(&other).ok_or_else(|| mlua::Error::RuntimeError("bad argument to Vector/Angle".into()))?;
            let _ = lua;
            Ok([x, b.unwrap_or(0.0), c.unwrap_or(0.0)])
        }
    }
}

/// Installs constructors and engine methods.
pub fn install(lua: &Lua) -> mlua::Result<()> {
    let g = lua.globals();
    g.set(
        "Vector",
        lua.create_function(|lua, (a, b, c): (Value, Option<f32>, Option<f32>)| Ok(Vector(args3(lua, a, b, c)?)))?,
    )?;
    g.set(
        "Angle",
        lua.create_function(|lua, (a, b, c): (Value, Option<f32>, Option<f32>)| Ok(Angle(args3(lua, a, b, c)?)))?,
    )?;

    let v: Table = meta::get(lua, "Vector")?;
    let f = |lua: &Lua, t: &Table, name: &str, func: mlua::Function| -> mlua::Result<()> {
        let _ = lua;
        t.raw_set(name, func)
    };
    f(lua, &v, "Length", lua.create_function(|_, a: Vector| Ok(len3(a.0)))?)?;
    f(lua, &v, "LengthSqr", lua.create_function(|_, a: Vector| Ok(a.0.iter().map(|c| c * c).sum::<f32>()))?)?;
    f(lua, &v, "Length2D", lua.create_function(|_, a: Vector| Ok((a.0[0] * a.0[0] + a.0[1] * a.0[1]).sqrt()))?)?;
    f(lua, &v, "Length2DSqr", lua.create_function(|_, a: Vector| Ok(a.0[0] * a.0[0] + a.0[1] * a.0[1]))?)?;
    f(lua, &v, "Distance", lua.create_function(|_, (a, b): (Vector, Vector)| {
        Ok(len3([a.0[0] - b.0[0], a.0[1] - b.0[1], a.0[2] - b.0[2]]))
    })?)?;
    f(lua, &v, "DistToSqr", lua.create_function(|_, (a, b): (Vector, Vector)| {
        let d = [a.0[0] - b.0[0], a.0[1] - b.0[1], a.0[2] - b.0[2]];
        Ok(d[0] * d[0] + d[1] * d[1] + d[2] * d[2])
    })?)?;
    f(lua, &v, "Dot", lua.create_function(|_, (a, b): (Vector, Vector)| {
        Ok(a.0[0] * b.0[0] + a.0[1] * b.0[1] + a.0[2] * b.0[2])
    })?)?;
    f(lua, &v, "Cross", lua.create_function(|_, (a, b): (Vector, Vector)| {
        let (a, b) = (a.0, b.0);
        Ok(Vector([a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]))
    })?)?;
    f(lua, &v, "GetNormalized", lua.create_function(|_, a: Vector| {
        let l = len3(a.0);
        Ok(if l > 0.0 { Vector([a.0[0] / l, a.0[1] / l, a.0[2] / l]) } else { Vector([0.0; 3]) })
    })?)?;
    v.raw_set("GetNormal", v.raw_get::<mlua::Function>("GetNormalized")?)?;
    f(lua, &v, "Normalize", lua.create_function(|_, ud: mlua::AnyUserData| {
        let mut a = ud.borrow_mut::<Vector>()?;
        let l = len3(a.0);
        if l > 0.0 {
            for c in a.0.iter_mut() {
                *c /= l;
            }
        }
        Ok(())
    })?)?;
    f(lua, &v, "Add", lua.create_function(|_, (ud, b): (mlua::AnyUserData, Vector)| {
        let mut a = ud.borrow_mut::<Vector>()?;
        for i in 0..3 {
            a.0[i] += b.0[i];
        }
        Ok(())
    })?)?;
    f(lua, &v, "Sub", lua.create_function(|_, (ud, b): (mlua::AnyUserData, Vector)| {
        let mut a = ud.borrow_mut::<Vector>()?;
        for i in 0..3 {
            a.0[i] -= b.0[i];
        }
        Ok(())
    })?)?;
    f(lua, &v, "Mul", lua.create_function(|_, (ud, s): (mlua::AnyUserData, f32)| {
        let mut a = ud.borrow_mut::<Vector>()?;
        for c in a.0.iter_mut() {
            *c *= s;
        }
        Ok(())
    })?)?;
    f(lua, &v, "Set", lua.create_function(|_, (ud, b): (mlua::AnyUserData, Vector)| {
        ud.borrow_mut::<Vector>()?.0 = b.0;
        Ok(())
    })?)?;
    f(lua, &v, "Zero", lua.create_function(|_, ud: mlua::AnyUserData| {
        ud.borrow_mut::<Vector>()?.0 = [0.0; 3];
        Ok(())
    })?)?;
    f(lua, &v, "SetUnpacked", lua.create_function(|_, (ud, x, y, z): (mlua::AnyUserData, f32, f32, f32)| {
        ud.borrow_mut::<Vector>()?.0 = [x, y, z];
        Ok(())
    })?)?;
    f(lua, &v, "Unpack", lua.create_function(|_, a: Vector| Ok((a.0[0], a.0[1], a.0[2])))?)?;
    f(lua, &v, "IsZero", lua.create_function(|_, a: Vector| Ok(a.0 == [0.0; 3]))?)?;
    f(lua, &v, "IsEqualTol", lua.create_function(|_, (a, b, tol): (Vector, Vector, f32)| {
        Ok((0..3).all(|i| (a.0[i] - b.0[i]).abs() <= tol))
    })?)?;
    f(lua, &v, "ToTable", lua.create_function(|lua, a: Vector| lua.create_sequence_from(a.0))?)?;
    f(lua, &v, "Angle", lua.create_function(|_, a: Vector| Ok(Angle(vector_angles(a.0))))?)?;
    f(lua, &v, "WithinAABox", lua.create_function(|_, (p, lo, hi): (Vector, Vector, Vector)| {
        Ok((0..3).all(|i| p.0[i] >= lo.0[i].min(hi.0[i]) && p.0[i] <= lo.0[i].max(hi.0[i])))
    })?)?;
    f(lua, &v, "Rotate", lua.create_function(|_, (ud, ang): (mlua::AnyUserData, Angle)| {
        let mut a = ud.borrow_mut::<Vector>()?;
        a.0 = rotate(a.0, ang.0);
        Ok(())
    })?)?;

    let a: Table = meta::get(lua, "Angle")?;
    f(lua, &a, "Forward", lua.create_function(|_, a: Angle| Ok(Vector(angle_vectors(a.0).0)))?)?;
    f(lua, &a, "Right", lua.create_function(|_, a: Angle| Ok(Vector(angle_vectors(a.0).1)))?)?;
    f(lua, &a, "Up", lua.create_function(|_, a: Angle| Ok(Vector(angle_vectors(a.0).2)))?)?;
    f(lua, &a, "Normalize", lua.create_function(|_, ud: mlua::AnyUserData| {
        let mut a = ud.borrow_mut::<Angle>()?;
        for c in a.0.iter_mut() {
            *c = normalize_angle(*c);
        }
        Ok(())
    })?)?;
    f(lua, &a, "Set", lua.create_function(|_, (ud, b): (mlua::AnyUserData, Angle)| {
        ud.borrow_mut::<Angle>()?.0 = b.0;
        Ok(())
    })?)?;
    f(lua, &a, "Zero", lua.create_function(|_, ud: mlua::AnyUserData| {
        ud.borrow_mut::<Angle>()?.0 = [0.0; 3];
        Ok(())
    })?)?;
    f(lua, &a, "Unpack", lua.create_function(|_, a: Angle| Ok((a.0[0], a.0[1], a.0[2])))?)?;
    f(lua, &a, "IsZero", lua.create_function(|_, a: Angle| Ok(a.0 == [0.0; 3]))?)?;
    f(lua, &a, "ToTable", lua.create_function(|lua, a: Angle| lua.create_sequence_from(a.0))?)?;
    f(lua, &a, "SetUnpacked", lua.create_function(|_, (ud, x, y, z): (mlua::AnyUserData, f32, f32, f32)| {
        ud.borrow_mut::<Angle>()?.0 = [x, y, z];
        Ok(())
    })?)?;
    f(lua, &a, "RotateAroundAxis", lua.create_function(|_, (ud, axis, deg): (mlua::AnyUserData, Vector, f32)| {
        let mut a = ud.borrow_mut::<Angle>()?;
        a.0 = rotate_around_axis(a.0, axis.0, deg);
        Ok(())
    })?)?;
    Ok(())
}

pub fn normalize_angle(a: f32) -> f32 {
    let mut a = a % 360.0;
    if a > 180.0 {
        a -= 360.0;
    } else if a < -180.0 {
        a += 360.0;
    }
    a
}

/// Forward, right, up for pitch/yaw/roll in degrees.
pub fn angle_vectors(a: [f32; 3]) -> ([f32; 3], [f32; 3], [f32; 3]) {
    let (sp, cp) = a[0].to_radians().sin_cos();
    let (sy, cy) = a[1].to_radians().sin_cos();
    let (sr, cr) = a[2].to_radians().sin_cos();
    (
        [cp * cy, cp * sy, -sp],
        [-sr * sp * cy + cr * sy, -sr * sp * sy - cr * cy, -sr * cp],
        [cr * sp * cy + sr * sy, cr * sp * sy - sr * cy, cr * cp],
    )
}

/// Pitch/yaw of a direction (roll 0).
pub fn vector_angles(v: [f32; 3]) -> [f32; 3] {
    if v[0] == 0.0 && v[1] == 0.0 {
        return [if v[2] > 0.0 { 270.0 } else { 90.0 }, 0.0, 0.0];
    }
    let mut yaw = v[1].atan2(v[0]).to_degrees();
    if yaw < 0.0 {
        yaw += 360.0;
    }
    let tmp = (v[0] * v[0] + v[1] * v[1]).sqrt();
    let mut pitch = (-v[2]).atan2(tmp).to_degrees();
    if pitch < 0.0 {
        pitch += 360.0;
    }
    [pitch, yaw, 0.0]
}

/// Local → world rotation of `v` by angles `a`.
pub fn rotate(v: [f32; 3], a: [f32; 3]) -> [f32; 3] {
    let (f, r, u) = angle_vectors(a);
    // the engine's basis: x = forward, y = left (= -right), z = up
    [
        f[0] * v[0] - r[0] * v[1] + u[0] * v[2],
        f[1] * v[0] - r[1] * v[1] + u[1] * v[2],
        f[2] * v[0] - r[2] * v[1] + u[2] * v[2],
    ]
}

fn rotate_around_axis(a: [f32; 3], axis: [f32; 3], deg: f32) -> [f32; 3] {
    // rotate the three basis vectors, then rebuild angles from forward/up
    let l = len3(axis);
    if l == 0.0 {
        return a;
    }
    let k = [axis[0] / l, axis[1] / l, axis[2] / l];
    let (s, c) = deg.to_radians().sin_cos();
    let rot = |v: [f32; 3]| -> [f32; 3] {
        let d = k[0] * v[0] + k[1] * v[1] + k[2] * v[2];
        let cr = [k[1] * v[2] - k[2] * v[1], k[2] * v[0] - k[0] * v[2], k[0] * v[1] - k[1] * v[0]];
        [
            v[0] * c + cr[0] * s + k[0] * d * (1.0 - c),
            v[1] * c + cr[1] * s + k[1] * d * (1.0 - c),
            v[2] * c + cr[2] * s + k[2] * d * (1.0 - c),
        ]
    };
    let (f, _, u) = angle_vectors(a);
    let (f, u) = (rot(f), rot(u));
    let mut out = vector_angles(f);
    // roll from the rotated up vector
    let (_, r0, u0) = angle_vectors([out[0], out[1], 0.0]);
    let roll = (-(u[0] * r0[0] + u[1] * r0[1] + u[2] * r0[2])).atan2(u[0] * u0[0] + u[1] * u0[1] + u[2] * u0[2]);
    out[2] = roll.to_degrees();
    out
}

#[cfg(test)]
mod tests {
    use crate::GLua;

    fn eval(code: &str) -> String {
        let g = GLua::new(std::path::Path::new(".")).unwrap();
        g.lua.load(code).eval::<String>().unwrap()
    }

    #[test]
    fn vector_basics() {
        assert_eq!(eval("local v = Vector(1,2,3) + Vector(1,1,1) return tostring(v)"), "2.000000 3.000000 4.000000");
        assert_eq!(eval("local v = Vector(3,4,0) return tostring(v:Length())"), "5");
        assert_eq!(eval("local v = Vector(1,2,3) v.x = 9 return tostring(v[1] + v.z)"), "12");
        assert_eq!(eval("return tostring(Vector(1,0,0) * 2 == Vector(2,0,0))"), "true");
        assert_eq!(eval("local m = FindMetaTable('Vector') function m:Twice() return self * 2 end return tostring(Vector(1,1,1):Twice())"), "2.000000 2.000000 2.000000");
        assert_eq!(eval("return getmetatable(Vector()).MetaName"), "Vector");
    }

    #[test]
    fn angle_basics() {
        assert_eq!(eval("local f = Angle(0,90,0):Forward() return string.format('%.3f %.3f', f.x, f.y)"), "-0.000 1.000");
        assert_eq!(eval("local a = Angle(10, 20, 30) return a.p + a.yaw + a[3]"), "60");
        assert_eq!(eval("return getmetatable(Angle()).MetaName"), "Angle");
        assert_eq!(eval("local a = Vector(0,1,0):Angle() return tostring(a.y)"), "90");
    }
}

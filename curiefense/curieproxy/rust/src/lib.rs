extern crate mlua;

use mlua::prelude::*;
use serde_json::json;
use std::collections::HashMap;

mod curiefense;

use curiefense::acl::{check_acl, ACLDecision, ACLResult, BotHuman};
use curiefense::config::hostmap::{HostMap, UrlMap};
use curiefense::config::{get_config, Config, HSDB};
use curiefense::interface::{
    challenge_phase01, challenge_phase02, Action, ActionType, Decision, Grasshopper,
};
use curiefense::limit::limit_check;
use curiefense::lua::{InspectionResult, LuaRequestInfo, Luagrasshopper};
use curiefense::session;
use curiefense::tagging::tag_request;
use curiefense::urlmap::match_urlmap;
use curiefense::utils::{ip_from_headers, map_request, RequestInfo};
use curiefense::waf::waf_check;

/// Lua/envoy entry point
fn inspect(
    lua: &Lua,
    args: (
        HashMap<String, String>,
        HashMap<String, LuaValue>,
        Option<LuaTable>,
    ),
) -> LuaResult<Option<InspectionResult>> {
    println!("ARGS: {:?}", args);

    let (metaheaders, metadata, lua_grasshopper) = args;
    let grasshopper = lua_grasshopper.map(Luagrasshopper);

    let hops: usize = metadata
        .get("xff_trusted_hops")
        .and_then(|v| FromLua::from_lua(v.clone(), lua).ok())
        .unwrap_or(1);
    let str_ip = ip_from_headers(&metaheaders, hops);

    let res = inspect_generic(grasshopper, "/config/current/config", str_ip, metaheaders);
    println!("Inspection result: {:?}", res);
    Ok(res.ok().map(InspectionResult))
}

fn lua_map_request(
    lua: &Lua,
    args: (HashMap<String, String>, HashMap<String, LuaValue>),
) -> LuaResult<LuaRequestInfo> {
    let (metaheaders, metadata) = args;

    let hops: usize = metadata
        .get("xff_trusted_hops")
        .and_then(|v| FromLua::from_lua(v.clone(), lua).ok())
        .unwrap_or(1);
    let str_ip = ip_from_headers(&metaheaders, hops);

    let rinfo = map_request(str_ip, metaheaders);

    Ok(LuaRequestInfo(rinfo))
}

fn acl_block(blocking: bool, code: i32, tags: &[String]) -> Decision {
    Decision::Action(Action {
        atype: if blocking {
            ActionType::Block
        } else {
            ActionType::Monitor
        },
        ban: false,
        status: 403,
        headers: None,
        reason: json!({"action": code, "initiator": "acl", "reason": tags }),
        content: "access denied".to_string(),
        extra_tags: None,
    })
}

fn challenge_verified<GH: Grasshopper>(gh: &GH, reqinfo: &RequestInfo) -> bool {
    if let Some(rbzid) = reqinfo.cookies.get("rbzid") {
        if let Some(ua) = reqinfo.headers.get("user-agent") {
            return gh
                .parse_rbzid(&rbzid.replace('-', "="), ua)
                .unwrap_or(false);
        }
    }
    false
}

/// generic entry point
/// this is not that generic, as we expect :path and :authority to be in metaheaders
fn inspect_generic<GH: Grasshopper>(
    mgh: Option<GH>,
    configpath: &str,
    ip_str: String,
    metaheaders: HashMap<String, String>,
) -> Result<Decision, Box<dyn std::error::Error>> {
    let cfg = get_config(configpath)?;
    let reqinfo = map_request(ip_str, metaheaders);
    let (nm, urlmap) = match match_urlmap(&reqinfo, &cfg) {
        None => return Ok(Decision::Pass),
        Some(x) => x,
    };

    if let Some(dec) = mgh.as_ref().and_then(|gh| {
        reqinfo
            .rinfo
            .qinfo
            .uri
            .as_ref()
            .and_then(|uri| challenge_phase02(gh, uri, &reqinfo.headers))
    }) {
        return Ok(dec);
    }

    let mut tags = tag_request(&cfg, &reqinfo);
    tags.insert_qualified("urlmap", &nm);
    tags.insert_qualified("urlmap-entry", &urlmap.name);
    tags.insert_qualified("aclid", &urlmap.acl_profile.id);
    tags.insert_qualified("aclname", &urlmap.acl_profile.name);
    tags.insert_qualified("wafid", &urlmap.waf_profile.name);

    // TODO challenge

    println!("REQINFO: {:?}", reqinfo);
    println!("urlmap: {:?}", urlmap);

    // limit checks, this is
    let limit_check = limit_check(&reqinfo, &urlmap.limits, &mut tags);
    println!("LIMIT_CHECKS: {:?}", limit_check);
    if let Decision::Action(_) = limit_check {
        // limit hit!
        return Ok(limit_check);
    }

    let acl_result = check_acl(&tags, &urlmap.acl_profile);
    println!("ACLRESULTS: {:?}", acl_result);
    match acl_result {
        ACLResult::Bypass(dec) => {
            if dec.allowed {
                return Ok(Decision::Pass);
            } else {
                return Ok(acl_block(urlmap.acl_active, 0, &dec.tags));
            }
        }
        // human blocked, always block, even if it is a bot
        ACLResult::Match(BotHuman {
            bot: _,
            human:
                Some(ACLDecision {
                    allowed: false,
                    tags,
                }),
        }) => return Ok(acl_block(urlmap.acl_active, 5, &tags)),
        // robot blocked, should be challenged, just block for now
        ACLResult::Match(BotHuman {
            bot:
                Some(ACLDecision {
                    allowed: false,
                    tags,
                }),
            human: _,
        }) => {
            // if grasshopper is available, run these tests
            if let Some(gh) = mgh {
                if !challenge_verified(&gh, &reqinfo) {
                    return Ok(match reqinfo.headers.get("user-agent") {
                        None => acl_block(urlmap.acl_active, 3, &tags),
                        Some(ua) => challenge_phase01(&gh, ua, tags),
                    });
                }
            }
        }
        _ => (),
    }
    let waf_result = waf_check(&reqinfo, &urlmap.waf_profile, HSDB.read()?);
    println!("WAFRESULTS: {:?}", waf_result);

    Ok(match waf_result {
        Ok(()) => Decision::Pass,
        Err(wb) => Decision::Action(wb.to_action()),
    })
}

/// wraps a result into a go-like pair
fn lua_result<R>(v: anyhow::Result<R>) -> LuaResult<(Option<R>, Option<String>)> {
    match v {
        Ok(x) => Ok((Some(x), None)),
        Err(rr) => Ok((None, Some(format!("{}", rr)))),
    }
}

/// runs the passed function, assuming the argument is a string
fn with_str<F, R>(lua: &Lua, session_id: LuaValue, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&str) -> anyhow::Result<R>,
{
    let decoded: String =
        FromLua::from_lua(session_id, lua).map_err(|rr| anyhow::anyhow!("{}", rr))?;
    f(&decoded)
}

/// runs the underlying string using function, catching mlua errors
fn wrap_session<F, R>(
    lua: &Lua,
    session_id: LuaValue,
    f: F,
) -> LuaResult<(Option<R>, Option<String>)>
where
    F: FnOnce(&str) -> anyhow::Result<R>,
{
    lua_result(with_str(lua, session_id, f))
}

/// runs the underlying string using, json returning, function, catching mlua errors
fn wrap_session_json<F, R: serde::Serialize>(
    lua: &Lua,
    session_id: LuaValue,
    f: F,
) -> LuaResult<(Option<String>, Option<String>)>
where
    F: FnOnce(&str) -> anyhow::Result<R>,
{
    lua_result(with_str(lua, session_id, |s| {
        f(s).and_then(|r| serde_json::to_string(&r).map_err(|rr| anyhow::anyhow!("{}", rr)))
    }))
}

#[mlua::lua_module]
fn curiefense(lua: &Lua) -> LuaResult<LuaTable> {
    let exports = lua.create_table()?;
    exports.set("inspect", lua.create_function(inspect)?)?;
    exports.set("map_request", lua.create_function(lua_map_request)?)?;

    // session functions
    exports.set(
        "init_config",
        lua.create_function(|_: &Lua, _: ()| lua_result(session::init_config()))?,
    )?;
    exports.set(
        "session_init",
        lua.create_function(|lua: &Lua, encoded_request_map: LuaValue| {
            wrap_session(lua, encoded_request_map, session::session_init)
        })?,
    )?;
    exports.set(
        "session_clean",
        lua.create_function(|lua: &Lua, session_id: LuaValue| {
            wrap_session(lua, session_id, |s| {
                session::clean_session(s).map(|()| true)
            })
        })?,
    )?;
    exports.set(
        "session_serialize_request_map",
        lua.create_function(|lua: &Lua, session_id: LuaValue| {
            wrap_session_json(lua, session_id, session::session_serialize_request_map)
        })?,
    )?;
    exports.set(
        "session_match_urlmap",
        lua.create_function(|lua: &Lua, session_id: LuaValue| {
            wrap_session_json(lua, session_id, session::session_match_urlmap)
        })?,
    )?;
    exports.set(
        "session_tag_request",
        lua.create_function(|lua: &Lua, session_id: LuaValue| {
            wrap_session_json(lua, session_id, session::session_tag_request)
        })?,
    )?;
    exports.set(
        "session_limit_check",
        lua.create_function(|lua: &Lua, session_id: LuaValue| {
            wrap_session_json(lua, session_id, session::session_limit_check)
        })?,
    )?;
    exports.set(
        "session_acl_check",
        lua.create_function(|lua: &Lua, session_id: LuaValue| {
            wrap_session_json(lua, session_id, session::session_acl_check)
        })?,
    )?;
    exports.set(
        "session_waf_check",
        lua.create_function(|lua: &Lua, session_id: LuaValue| {
            wrap_session_json(lua, session_id, session::session_waf_check)
        })?,
    )?;

    exports.set(
        "new_ip_set",
        lua.create_function(curiefense::iptools::new_ip_set)?,
    )?;
    exports.set(
        "new_sig_set",
        lua.create_function(curiefense::iptools::new_sig_set)?,
    )?;
    exports.set(
        "new_geoipdb",
        lua.create_function(curiefense::iptools::new_geoipdb)?,
    )?;
    exports.set(
        "modhash",
        lua.create_function(curiefense::iptools::modhash)?,
    )?;
    exports.set(
        "iptonum",
        lua.create_function(curiefense::iptools::iptonum)?,
    )?;
    exports.set(
        "decodeurl",
        lua.create_function(curiefense::iptools::decodeurl)?,
    )?;
    exports.set(
        "encodeurl",
        lua.create_function(curiefense::iptools::encodeurl)?,
    )?;
    exports.set(
        "test_regex",
        lua.create_function(curiefense::iptools::test_regex)?,
    )?;

    Ok(exports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_load() {
        let r = get_config("../mounts/config/current/config");
        assert!(r.is_ok(), format!("{:?}", r));
    }
}
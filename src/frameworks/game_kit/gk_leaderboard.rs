/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0.
 * If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `GKLeaderboard`.

use crate::frameworks::foundation::{ns_array, NSInteger, NSRange, NSUInteger};
use crate::objc::{
    autorelease, id, msg, nil, objc_classes, release, retain, ClassExports, HostObject, NSZonePtr,
};

#[derive(Default)]
struct GKLeaderboardHostObject {
    category: id,
    identifier: id,
    title: id,
    time_scope: NSInteger,
    player_scope: NSInteger,
    range: NSRange,
}
impl HostObject for GKLeaderboardHostObject {}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation GKLeaderboard: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(GKLeaderboardHostObject {
        category: nil,
        identifier: nil,
        title: nil,
        time_scope: 2, // GKLeaderboardTimeScopeAllTime
        player_scope: 0, // GKLeaderboardPlayerScopeGlobal
        range: NSRange { location: 1, length: 25 },
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

+ (())loadLeaderboardsWithCompletionHandler:(id)handler {
    if handler != nil {
        let empty = ns_array::from_vec(env, Vec::new());
        autorelease(env, empty);
        let invoke_ptr_addr: crate::mem::ConstPtr<crate::abi::GuestFunction> =
            crate::mem::Ptr::from_bits(handler.to_bits() + 12);
        if env.mem.get_bytes_fallible(invoke_ptr_addr.cast(), 4).is_some() {
            let invoke_func = env.mem.read(invoke_ptr_addr);
            if invoke_func.addr_without_thumb_bit() != 0 {
                use crate::abi::CallFromHost;
                let _: () = invoke_func.call_from_host(env, (handler, empty, nil));
            }
        }
    }
}

+ (())loadCategoriesWithCompletionHandler:(id)handler {
    if handler != nil {
        let empty1 = ns_array::from_vec(env, Vec::new());
        autorelease(env, empty1);
        let empty2 = ns_array::from_vec(env, Vec::new());
        autorelease(env, empty2);
        let invoke_ptr_addr: crate::mem::ConstPtr<crate::abi::GuestFunction> =
            crate::mem::Ptr::from_bits(handler.to_bits() + 12);
        if env.mem.get_bytes_fallible(invoke_ptr_addr.cast(), 4).is_some() {
            let invoke_func = env.mem.read(invoke_ptr_addr);
            if invoke_func.addr_without_thumb_bit() != 0 {
                use crate::abi::CallFromHost;
                let _: () = invoke_func.call_from_host(env, (handler, empty1, empty2, nil));
            }
        }
    }
}

+ (())setDefaultLeaderboard:(id)_leaderboard_identifier withCompletionHandler:(id)handler {
    if handler != nil {
        let invoke_ptr_addr: crate::mem::ConstPtr<crate::abi::GuestFunction> =
            crate::mem::Ptr::from_bits(handler.to_bits() + 12);
        if env.mem.get_bytes_fallible(invoke_ptr_addr.cast(), 4).is_some() {
            let invoke_func = env.mem.read(invoke_ptr_addr);
            if invoke_func.addr_without_thumb_bit() != 0 {
                use crate::abi::CallFromHost;
                let _: () = invoke_func.call_from_host(env, (handler, nil));
            }
        }
    }
}

- (id)init {
    this
}

- (id)initWithPlayerIDs:(id)_player_ids {
    this
}

- (id)initWithPlayers:(id)_players {
    this
}

- (id)category {
    env.objc.borrow::<GKLeaderboardHostObject>(this).category
}

- (())setCategory:(id)category {
    if category != nil { retain(env, category); }
    let old = std::mem::replace(&mut env.objc.borrow_mut::<GKLeaderboardHostObject>(this).category, category);
    if old != nil { release(env, old); }
}

- (id)identifier {
    let host = env.objc.borrow::<GKLeaderboardHostObject>(this);
    if host.identifier != nil { host.identifier } else { host.category }
}

- (())setIdentifier:(id)identifier {
    if identifier != nil { retain(env, identifier); }
    let old = std::mem::replace(&mut env.objc.borrow_mut::<GKLeaderboardHostObject>(this).identifier, identifier);
    if old != nil { release(env, old); }
}

- (id)title {
    env.objc.borrow::<GKLeaderboardHostObject>(this).title
}

- (NSInteger)timeScope {
    env.objc.borrow::<GKLeaderboardHostObject>(this).time_scope
}

- (())setTimeScope:(NSInteger)scope {
    env.objc.borrow_mut::<GKLeaderboardHostObject>(this).time_scope = scope;
}

- (NSInteger)playerScope {
    env.objc.borrow::<GKLeaderboardHostObject>(this).player_scope
}

- (())setPlayerScope:(NSInteger)scope {
    env.objc.borrow_mut::<GKLeaderboardHostObject>(this).player_scope = scope;
}

- (NSRange)range {
    env.objc.borrow::<GKLeaderboardHostObject>(this).range
}

- (())setRange:(NSRange)range {
    env.objc.borrow_mut::<GKLeaderboardHostObject>(this).range = range;
}

- (NSUInteger)maxRange {
    0
}

- (id)scores {
    nil
}

- (id)localPlayerScore {
    nil
}

- (bool)isLoading {
    false
}

- (())loadScoresWithCompletionHandler:(id)handler {
    if handler != nil {
        let invoke_ptr_addr: crate::mem::ConstPtr<crate::abi::GuestFunction> =
            crate::mem::Ptr::from_bits(handler.to_bits() + 12);
        if env.mem.get_bytes_fallible(invoke_ptr_addr.cast(), 4).is_some() {
            let invoke_func = env.mem.read(invoke_ptr_addr);
            if invoke_func.addr_without_thumb_bit() != 0 {
                use crate::abi::CallFromHost;
                let _: () = invoke_func.call_from_host(env, (handler, nil, nil));
            }
        }
    }
}

- (())dealloc {
    let &GKLeaderboardHostObject { category, identifier, title, .. } = env.objc.borrow(this);
    if category != nil { release(env, category); }
    if identifier != nil { release(env, identifier); }
    if title != nil { release(env, title); }
    env.objc.dealloc_object(this, &mut env.mem);
}

@end

};

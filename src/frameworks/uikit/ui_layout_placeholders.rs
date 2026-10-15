/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Auto Layout / UIFontDescriptor / NSLocalizableString placeholders.
//!
//! HyperHLE renders the screen using a frame-based UI (everything is
//! positioned through `frame`, `bounds` and `center` on `UIView`). It does
//! not implement Apple's Cascade Auto Layout engine. However, modern
//! storyboards and xibs frequently reference layout-related classes from
//! within their NIB archive — most notably:
//!
//! * `NSLayoutConstraint` (a constraint between two anchors),
//! * `_UILayoutGuide` (a non-drawing layout area, e.g. the top/bottom
//!   layout guide of a view controller),
//! * `_UILayoutSupportConstraint` (a private constraint helper Apple uses
//!   to anchor the layout guides),
//! * `UIFontDescriptor` (font information stored in nibs).
//!
//! Without `initWithCoder:` implementations for those classes the NIB
//! decoder raises `does not respond to selector initWithCoder:` and the
//! whole storyboard scene fails to load. The implementations here record
//! every decoded attribute on a host-side dictionary so that introspection
//! and KVC still work, but they intentionally do not run a constraint
//! solver — the view controllers receive plain `UIView`s that use the
//! frames declared elsewhere in the nib instead.
//!
//! `NSLocalizableString` is exported by the legacy Foundation private API
//! as a thin `NSString` subclass; we keep behaviour parity by inheriting
//! straight from `NSString` and forwarding `initWithCoder:` to the
//! existing NSString unarchival path.

use crate::frameworks::core_graphics::CGFloat;
use crate::frameworks::foundation::ns_string::get_static_str;
use crate::objc::{
    autorelease, id, msg, msg_class, nil, objc_classes, release, retain, ClassExports, HostObject,
    NSZonePtr,
};

#[derive(Default)]
struct NSLayoutConstraintHostObject {
    first_item: id,
    first_attribute: i32,
    relation: i32,
    second_item: id,
    second_attribute: i32,
    multiplier: CGFloat,
    constant: CGFloat,
    priority: f32,
    identifier: id,
    active: bool,
}
impl HostObject for NSLayoutConstraintHostObject {}

#[derive(Default)]
struct UILayoutGuideHostObject {
    /// Owning view (non-retained), set when the guide is added to a view
    /// via `-addLayoutGuide:`. Always nil under the no-op constraint
    /// engine.
    owning_view: id,
    identifier: id,
    /// Most code that asks for `frame` immediately afterwards is happy
    /// with `CGRectZero`; we still store any frame the nib decoded so
    /// `setFrame:` round-trips.
    frame_origin_x: CGFloat,
    frame_origin_y: CGFloat,
    frame_size_w: CGFloat,
    frame_size_h: CGFloat,
}
impl HostObject for UILayoutGuideHostObject {}

#[derive(Default)]
struct UIFontDescriptorHostObject {
    attributes: id,
    point_size: CGFloat,
}
impl HostObject for UIFontDescriptorHostObject {}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

// =========================================================================
// MARK: - NSLayoutConstraint
// =========================================================================

@implementation NSLayoutConstraint: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host = Box::<NSLayoutConstraintHostObject>::default();
    env.objc.alloc_object(this, host, &mut env.mem)
}

+ (id)constraintWithItem:(id)first_item
                 attribute:(i32)first_attribute
                 relatedBy:(i32)relation
                    toItem:(id)second_item
                 attribute:(i32)second_attribute
                multiplier:(CGFloat)multiplier
                  constant:(CGFloat)constant {
    let constraint: id = msg![env; this alloc];
    if first_item != nil { retain(env, first_item); }
    if second_item != nil { retain(env, second_item); }
    {
        let host = env.objc.borrow_mut::<NSLayoutConstraintHostObject>(constraint);
        host.first_item = first_item;
        host.first_attribute = first_attribute;
        host.relation = relation;
        host.second_item = second_item;
        host.second_attribute = second_attribute;
        host.multiplier = multiplier;
        host.constant = constant;
        host.priority = 1000.0;
        host.active = true;
    }
    autorelease(env, constraint)
}

// Bulk activation helpers used by NSLayoutConstraint-heavy code.
+ (())activateConstraints:(id)constraints {
    if constraints == nil { return; }
    let count: crate::frameworks::foundation::NSUInteger = msg![env; constraints count];
    for i in 0..count {
        let c: id = msg![env; constraints objectAtIndex:i];
        let _: () = msg![env; c setActive:true];
    }
}
+ (())deactivateConstraints:(id)constraints {
    if constraints == nil { return; }
    let count: crate::frameworks::foundation::NSUInteger = msg![env; constraints count];
    for i in 0..count {
        let c: id = msg![env; constraints objectAtIndex:i];
        let _: () = msg![env; c setActive:false];
    }
}

- (id)initWithCoder:(id)coder {
    let first_item_key = get_static_str(env, "NSFirstItem");
    let first_item: id = msg![env; coder decodeObjectForKey:first_item_key];
    if first_item != nil { retain(env, first_item); }

    let second_item_key = get_static_str(env, "NSSecondItem");
    let second_item: id = msg![env; coder decodeObjectForKey:second_item_key];
    if second_item != nil { retain(env, second_item); }

    let first_attr_key = get_static_str(env, "NSFirstAttribute");
    let first_attr: i32 = msg![env; coder decodeIntForKey:first_attr_key];

    let second_attr_key = get_static_str(env, "NSSecondAttribute");
    let second_attr: i32 = msg![env; coder decodeIntForKey:second_attr_key];

    let relation_key = get_static_str(env, "NSRelation");
    let relation: i32 = msg![env; coder decodeIntForKey:relation_key];

    let multiplier_key = get_static_str(env, "NSMultiplier");
    let multiplier: f32 = msg![env; coder decodeFloatForKey:multiplier_key];

    let constant_key = get_static_str(env, "NSConstant");
    let constant: f32 = msg![env; coder decodeFloatForKey:constant_key];

    let priority_key = get_static_str(env, "NSPriority");
    let mut priority: f32 = msg![env; coder decodeFloatForKey:priority_key];
    if priority == 0.0 { priority = 1000.0; }

    let identifier_key = get_static_str(env, "NSIdentifier");
    let identifier: id = msg![env; coder decodeObjectForKey:identifier_key];
    if identifier != nil { retain(env, identifier); }

    {
        let host = env.objc.borrow_mut::<NSLayoutConstraintHostObject>(this);
        host.first_item = first_item;
        host.first_attribute = first_attr;
        host.second_item = second_item;
        host.second_attribute = second_attr;
        host.relation = relation;
        host.multiplier = multiplier;
        host.constant = constant;
        host.priority = priority;
        host.identifier = identifier;
        host.active = true;
    }
    this
}

- (id)firstItem {
    env.objc.borrow::<NSLayoutConstraintHostObject>(this).first_item
}
- (id)secondItem {
    env.objc.borrow::<NSLayoutConstraintHostObject>(this).second_item
}
- (i32)firstAttribute {
    env.objc.borrow::<NSLayoutConstraintHostObject>(this).first_attribute
}
- (i32)secondAttribute {
    env.objc.borrow::<NSLayoutConstraintHostObject>(this).second_attribute
}
- (i32)relation {
    env.objc.borrow::<NSLayoutConstraintHostObject>(this).relation
}
- (CGFloat)multiplier {
    env.objc.borrow::<NSLayoutConstraintHostObject>(this).multiplier
}
- (CGFloat)constant {
    env.objc.borrow::<NSLayoutConstraintHostObject>(this).constant
}
- (())setConstant:(CGFloat)c {
    env.objc.borrow_mut::<NSLayoutConstraintHostObject>(this).constant = c;
}
- (f32)priority {
    env.objc.borrow::<NSLayoutConstraintHostObject>(this).priority
}
- (())setPriority:(f32)p {
    env.objc.borrow_mut::<NSLayoutConstraintHostObject>(this).priority = p;
}
- (id)identifier {
    env.objc.borrow::<NSLayoutConstraintHostObject>(this).identifier
}
- (())setIdentifier:(id)identifier {
    if identifier != nil { retain(env, identifier); }
    let host = env.objc.borrow_mut::<NSLayoutConstraintHostObject>(this);
    let old = host.identifier;
    host.identifier = identifier;
    if old != nil { release(env, old); }
}
- (bool)isActive {
    env.objc.borrow::<NSLayoutConstraintHostObject>(this).active
}
- (())setActive:(bool)active {
    env.objc.borrow_mut::<NSLayoutConstraintHostObject>(this).active = active;
}

- (())dealloc {
    let &NSLayoutConstraintHostObject { first_item, second_item, identifier, .. } =
        env.objc.borrow(this);
    if first_item != nil { release(env, first_item); }
    if second_item != nil { release(env, second_item); }
    if identifier != nil { release(env, identifier); }
    env.objc.dealloc_object(this, &mut env.mem)
}

@end

// =========================================================================
// MARK: - _UILayoutSupportConstraint
// =========================================================================
//
// `_UILayoutSupportConstraint` is a private Apple subclass of
// `NSLayoutConstraint`. Behaviour is identical for our purposes.

@implementation _UILayoutSupportConstraint: NSLayoutConstraint
@end

// =========================================================================
// MARK: - _UILayoutGuide / UILayoutGuide
// =========================================================================
//
// `_UILayoutGuide` is the legacy private name (pre-iOS 9). `UILayoutGuide`
// is the public class introduced in iOS 9. Both behave as non-drawing
// guides positioned via Auto Layout. HyperHLE does not run a constraint
// solver, so the guides remain at zero frame; storyboards that reference
// `topLayoutGuide` / `bottomLayoutGuide` still decode correctly though.

@implementation _UILayoutGuide: UIView
- (CGFloat)length {
    0.0
}
- (id)topAnchor { this }
- (id)bottomAnchor { this }
- (id)heightAnchor { this }
@end

@implementation UILayoutGuide: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host = Box::<UILayoutGuideHostObject>::default();
    env.objc.alloc_object(this, host, &mut env.mem)
}

- (id)init { this }

- (id)initWithCoder:(id)coder {
    let identifier_key = get_static_str(env, "UILayoutGuideIdentifier");
    let identifier: id = msg![env; coder decodeObjectForKey:identifier_key];
    if identifier != nil { retain(env, identifier); }
    env.objc.borrow_mut::<UILayoutGuideHostObject>(this).identifier = identifier;
    this
}

- (id)identifier {
    env.objc.borrow::<UILayoutGuideHostObject>(this).identifier
}
- (())setIdentifier:(id)identifier {
    if identifier != nil { retain(env, identifier); }
    let host = env.objc.borrow_mut::<UILayoutGuideHostObject>(this);
    let old = host.identifier;
    host.identifier = identifier;
    if old != nil { release(env, old); }
}

- (CGFloat)length {
    0.0
}

- (id)owningView {
    env.objc.borrow::<UILayoutGuideHostObject>(this).owning_view
}
- (())setOwningView:(id)view {
    env.objc.borrow_mut::<UILayoutGuideHostObject>(this).owning_view = view;
}

- (crate::frameworks::core_graphics::CGRect)layoutFrame {
    let host = env.objc.borrow::<UILayoutGuideHostObject>(this);
    crate::frameworks::core_graphics::CGRect {
        origin: crate::frameworks::core_graphics::CGPoint {
            x: host.frame_origin_x,
            y: host.frame_origin_y,
        },
        size: crate::frameworks::core_graphics::CGSize {
            width: host.frame_size_w,
            height: host.frame_size_h,
        },
    }
}

- (())dealloc {
    let &UILayoutGuideHostObject { identifier, .. } = env.objc.borrow(this);
    if identifier != nil { release(env, identifier); }
    env.objc.dealloc_object(this, &mut env.mem)
}

@end

// =========================================================================
// MARK: - UIFontDescriptor
// =========================================================================

@implementation UIFontDescriptor: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host = Box::<UIFontDescriptorHostObject>::default();
    env.objc.alloc_object(this, host, &mut env.mem)
}

+ (id)fontDescriptorWithFontAttributes:(id)attributes {
    let descriptor: id = msg![env; this alloc];
    if attributes != nil { retain(env, attributes); }
    env.objc.borrow_mut::<UIFontDescriptorHostObject>(descriptor).attributes = attributes;
    autorelease(env, descriptor)
}

+ (id)fontDescriptorWithName:(id)name size:(CGFloat)size {
    let attrs: id = msg_class![env; NSMutableDictionary dictionary];
    let name_key = get_static_str(env, "NSFontNameAttribute");
    let _: () = msg![env; attrs setObject:name forKey:name_key];
    let descriptor: id = msg![env; this fontDescriptorWithFontAttributes:attrs];
    env.objc.borrow_mut::<UIFontDescriptorHostObject>(descriptor).point_size = size;
    descriptor
}

+ (id)preferredFontDescriptorWithTextStyle:(id)style {
    let font: id = msg_class![env; UIFont preferredFontForTextStyle:style];
    if font != nil {
        msg![env; font fontDescriptor]
    } else {
        let name = get_static_str(env, "Helvetica");
        msg![env; this fontDescriptorWithName:name size:(17.0 as CGFloat)]
    }
}

- (id)initWithCoder:(id)coder {
    let attrs_key = get_static_str(env, "UIFontDescriptorAttributes");
    let attrs: id = msg![env; coder decodeObjectForKey:attrs_key];
    if attrs != nil { retain(env, attrs); }
    env.objc.borrow_mut::<UIFontDescriptorHostObject>(this).attributes = attrs;

    let size_key = get_static_str(env, "UIFontDescriptorPointSize");
    let size: f32 = msg![env; coder decodeFloatForKey:size_key];
    env.objc.borrow_mut::<UIFontDescriptorHostObject>(this).point_size = size;
    this
}

- (id)fontAttributes {
    env.objc.borrow::<UIFontDescriptorHostObject>(this).attributes
}
- (CGFloat)pointSize {
    env.objc.borrow::<UIFontDescriptorHostObject>(this).point_size
}
- (id)objectForKey:(id)key {
    let attrs = env.objc.borrow::<UIFontDescriptorHostObject>(this).attributes;
    if attrs == nil { return nil; }
    msg![env; attrs objectForKey:key]
}

- (id)fontDescriptorWithSymbolicTraits:(u32)_symbolic_traits {
    retain(env, this);
    autorelease(env, this)
}

- (u32)symbolicTraits {
    0
}

- (id)fontDescriptorWithAddingAttributes:(id)_attributes {
    retain(env, this);
    autorelease(env, this)
}

- (id)fontDescriptorWithSize:(CGFloat)size {
    let class = msg![env; this class];
    let attrs = env.objc.borrow::<UIFontDescriptorHostObject>(this).attributes;
    let descriptor: id = msg![env; class fontDescriptorWithFontAttributes:attrs];
    env.objc.borrow_mut::<UIFontDescriptorHostObject>(descriptor).point_size = size;
    descriptor
}

- (())dealloc {
    let &UIFontDescriptorHostObject { attributes, .. } = env.objc.borrow(this);
    if attributes != nil { release(env, attributes); }
    env.objc.dealloc_object(this, &mut env.mem)
}

@end

// =========================================================================
// MARK: - NSLocalizableString
// =========================================================================
//
// Some old Apple SDKs ship a private NSLocalizableString class that
// behaves as a thin NSString subclass which decodes from `NS.bytes`.
// Treat it as an alias of NSString so storyboard-encoded localized
// strings continue to work.

@implementation NSLocalizableString: NSString
@end

// =========================================================================
// MARK: - iOS 7 TextKit, NSStringDrawingContext, UIMotionEffect & UIDynamics
// =========================================================================

@implementation NSStringDrawingContext: NSObject
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
- (id)init { this }
- (CGFloat)minimumScaleFactor { 0.0 }
- (())setMinimumScaleFactor:(CGFloat)_factor { }
- (CGFloat)actualScaleFactor { 1.0 }
- (crate::frameworks::core_graphics::CGRect)totalBounds {
    crate::frameworks::core_graphics::CGRect::default()
}
@end

@implementation NSTextContainer: NSObject
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
- (id)init { this }
- (id)initWithSize:(crate::frameworks::core_graphics::CGSize)_size { this }
- (id)initWithCoder:(id)_coder { this }
- (())setLineFragmentPadding:(CGFloat)_padding { }
- (CGFloat)lineFragmentPadding { 5.0 }
- (())setMaximumNumberOfLines:(u32)_lines { }
- (u32)maximumNumberOfLines { 0 }
- (())setLineBreakMode:(i32)_mode { }
- (i32)lineBreakMode { 0 }
- (crate::frameworks::core_graphics::CGSize)size {
    crate::frameworks::core_graphics::CGSize { width: 10000.0, height: 10000.0 }
}
- (())setSize:(crate::frameworks::core_graphics::CGSize)_size { }
@end

@implementation NSLayoutManager: NSObject
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
- (id)init { this }
- (id)initWithCoder:(id)_coder { this }
- (())addTextContainer:(id)_container { }
- (id)textContainers { msg_class![env; NSArray array] }
- (crate::frameworks::core_graphics::CGRect)usedRectForTextContainer:(id)_container {
    crate::frameworks::core_graphics::CGRect::default()
}
@end

@implementation NSTextStorage: NSMutableAttributedString
@end

@implementation UIMotionEffect: NSObject
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
- (id)init { this }
- (id)initWithCoder:(id)_coder { this }
- (id)copyWithZone:(NSZonePtr)_zone { retain(env, this) }
@end

@implementation UIInterpolatingMotionEffect: UIMotionEffect
- (id)initWithKeyPath:(id)_key_path type:(i32)_type { this }
- (id)minimumRelativeValue { nil }
- (())setMinimumRelativeValue:(id)_val { }
- (id)maximumRelativeValue { nil }
- (())setMaximumRelativeValue:(id)_val { }
@end

@implementation UIMotionEffectGroup: UIMotionEffect
- (id)motionEffects { msg_class![env; NSArray array] }
- (())setMotionEffects:(id)_effects { }
@end

@implementation UIDynamicAnimator: NSObject
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
- (id)init { this }
- (id)initWithReferenceView:(id)_view { this }
- (id)initWithCollectionViewLayout:(id)_layout { this }
- (())addBehavior:(id)_behavior { }
- (())removeBehavior:(id)_behavior { }
- (())removeAllBehaviors { }
- (id)behaviors { msg_class![env; NSArray array] }
- (bool)isRunning { false }
- (f64)elapsedTime { 0.0 }
- (id)delegate { nil }
- (())setDelegate:(id)_delegate { }
- (id)referenceView { nil }
- (())updateItemUsingCurrentState:(id)_item { }
@end

@implementation UIDynamicBehavior: NSObject
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
- (id)init { this }
- (())addChildBehavior:(id)_behavior { }
- (())removeChildBehavior:(id)_behavior { }
- (id)childBehaviors { msg_class![env; NSArray array] }
- (id)action { nil }
- (())setAction:(id)_action { }
@end

@implementation UIGravityBehavior: UIDynamicBehavior
- (id)initWithItems:(id)_items { this }
- (())addItem:(id)_item { }
- (())removeItem:(id)_item { }
- (())setAngle:(CGFloat)_angle magnitude:(CGFloat)_magnitude { }
- (())setGravityDirection:(crate::frameworks::core_graphics::CGPoint)_dir { }
- (())setMagnitude:(CGFloat)_mag { }
- (())setAngle:(CGFloat)_angle { }
@end

@implementation UICollisionBehavior: UIDynamicBehavior
- (id)initWithItems:(id)_items { this }
- (())addItem:(id)_item { }
- (())removeItem:(id)_item { }
- (())setTranslatesReferenceBoundsIntoBoundary:(bool)_flag { }
- (bool)translatesReferenceBoundsIntoBoundary { false }
- (())setCollisionMode:(u32)_mode { }
- (u32)collisionMode { 0 }
- (())setCollisionDelegate:(id)_delegate { }
- (id)collisionDelegate { nil }
@end

@implementation UIPushBehavior: UIDynamicBehavior
- (id)initWithItems:(id)_items mode:(i32)_mode { this }
- (())addItem:(id)_item { }
- (())removeItem:(id)_item { }
- (())setActive:(bool)_active { }
- (bool)active { false }
- (())setAngle:(CGFloat)_angle magnitude:(CGFloat)_magnitude { }
- (())setMagnitude:(CGFloat)_mag { }
- (())setAngle:(CGFloat)_angle { }
- (())setPushDirection:(crate::frameworks::core_graphics::CGPoint)_dir { }
@end

@implementation UISnapBehavior: UIDynamicBehavior
- (id)initWithItem:(id)_item snapToPoint:(crate::frameworks::core_graphics::CGPoint)_point { this }
- (())setDamping:(CGFloat)_damping { }
- (CGFloat)damping { 0.5 }
@end

@implementation UIAttachmentBehavior: UIDynamicBehavior
- (id)initWithItem:(id)_item attachedToAnchor:(crate::frameworks::core_graphics::CGPoint)_point { this }
- (id)initWithItem:(id)_item1 attachedToItem:(id)_item2 { this }
- (())setLength:(CGFloat)_len { }
- (CGFloat)length { 0.0 }
- (())setDamping:(CGFloat)_damping { }
- (CGFloat)damping { 0.0 }
- (())setFrequency:(CGFloat)_freq { }
- (CGFloat)frequency { 0.0 }
@end

@implementation UIDynamicItemBehavior: UIDynamicBehavior
- (id)initWithItems:(id)_items { this }
- (())addItem:(id)_item { }
- (())removeItem:(id)_item { }
- (())setElasticity:(CGFloat)_val { }
- (CGFloat)elasticity { 0.0 }
- (())setFriction:(CGFloat)_val { }
- (CGFloat)friction { 0.2 }
- (())setDensity:(CGFloat)_val { }
- (CGFloat)density { 1.0 }
- (())setResistance:(CGFloat)_val { }
- (CGFloat)resistance { 0.0 }
- (())setAngularResistance:(CGFloat)_val { }
- (CGFloat)angularResistance { 0.0 }
- (())setAllowsRotation:(bool)_flag { }
- (bool)allowsRotation { true }
@end

// =========================================================================
// MARK: - iOS 7 SpriteKit & MultipeerConnectivity Stubs
// =========================================================================

@implementation SKView: UIView
- (())presentScene:(id)_scene { }
- (())presentScene:(id)_scene transition:(id)_transition { }
- (id)scene { nil }
- (bool)isPaused { false }
- (())setPaused:(bool)_paused { }
- (bool)showsFPS { false }
- (())setShowsFPS:(bool)_shows { }
- (bool)showsDrawCount { false }
- (())setShowsDrawCount:(bool)_shows { }
- (bool)showsNodeCount { false }
- (())setShowsNodeCount:(bool)_shows { }
- (bool)showsPhysics { false }
- (())setShowsPhysics:(bool)_shows { }
- (bool)ignoresSiblingOrder { false }
- (())setIgnoresSiblingOrder:(bool)_ignores { }
- (i32)frameInterval { 1 }
- (())setFrameInterval:(i32)_interval { }
- (id)textureFromNode:(id)_node { nil }
@end

@implementation SKNode: UIResponder
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
+ (id)node {
    let n: id = msg![env; this alloc];
    let n: id = msg![env; n init];
    autorelease(env, n)
}
- (id)init { this }
- (id)initWithCoder:(id)_coder { this }
- (())addChild:(id)_node { }
- (())insertChild:(id)_node atIndex:(i32)_index { }
- (())removeChildrenInArray:(id)_nodes { }
- (())removeAllChildren { }
- (())removeFromParent { }
- (id)childNodeWithName:(id)_name { nil }
- (id)children { msg_class![env; NSArray array] }
- (id)parent { nil }
- (id)scene { nil }
- (())runAction:(id)_action { }
- (())runAction:(id)_action withKey:(id)_key { }
- (())runAction:(id)_action completion:(id)block {
    if block != nil {
        let invoke_ptr_addr: crate::mem::ConstPtr<crate::abi::GuestFunction> =
            crate::mem::Ptr::from_bits(block.to_bits() + 12);
        if env.mem.get_bytes_fallible(invoke_ptr_addr.cast(), 4).is_some() {
            let invoke_func = env.mem.read(invoke_ptr_addr);
            if invoke_func.addr_without_thumb_bit() != 0 {
                use crate::abi::CallFromHost;
                let _: () = invoke_func.call_from_host(env, (block,));
            }
        }
    }
}
- (())removeActionForKey:(id)_key { }
- (())removeAllActions { }
- (bool)hasActions { false }
@end

@implementation SKScene: SKNode
+ (id)sceneWithSize:(crate::frameworks::core_graphics::CGSize)_size {
    let s: id = msg![env; this alloc];
    let s: id = msg![env; s init];
    autorelease(env, s)
}
- (id)initWithSize:(crate::frameworks::core_graphics::CGSize)_size { this }
- (i32)scaleMode { 0 }
- (())setScaleMode:(i32)_mode { }
- (id)view { nil }
@end

@implementation SKSpriteNode: SKNode
+ (id)spriteNodeWithImageNamed:(id)_name {
    let n: id = msg![env; this alloc];
    let n: id = msg![env; n init];
    autorelease(env, n)
}
+ (id)spriteNodeWithTexture:(id)_texture {
    let n: id = msg![env; this alloc];
    let n: id = msg![env; n init];
    autorelease(env, n)
}
+ (id)spriteNodeWithColor:(id)_color size:(crate::frameworks::core_graphics::CGSize)_size {
    let n: id = msg![env; this alloc];
    let n: id = msg![env; n init];
    autorelease(env, n)
}
- (id)initWithImageNamed:(id)_name { this }
- (id)initWithTexture:(id)_texture { this }
- (id)initWithTexture:(id)_texture color:(id)_color size:(crate::frameworks::core_graphics::CGSize)_size { this }
- (id)initWithColor:(id)_color size:(crate::frameworks::core_graphics::CGSize)_size { this }
@end

@implementation SKLabelNode: SKNode
+ (id)labelNodeWithFontNamed:(id)_font_name {
    let n: id = msg![env; this alloc];
    let n: id = msg![env; n init];
    autorelease(env, n)
}
- (id)initWithFontNamed:(id)_font_name { this }
@end

@implementation SKShapeNode: SKNode
@end

@implementation SKEffectNode: SKNode
@end

@implementation SKEmitterNode: SKNode
@end

@implementation SKCropNode: SKNode
@end

@implementation SKVideoNode: SKNode
@end

@implementation SKAction: NSObject
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
- (id)init { this }
- (id)copyWithZone:(NSZonePtr)_zone { retain(env, this) }
@end

@implementation SKTexture: NSObject
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
+ (id)textureWithImageNamed:(id)_name {
    let t: id = msg![env; this alloc];
    let t: id = msg![env; t init];
    autorelease(env, t)
}
- (id)init { this }
- (crate::frameworks::core_graphics::CGSize)size {
    crate::frameworks::core_graphics::CGSize { width: 64.0, height: 64.0 }
}
@end

@implementation SKTextureAtlas: NSObject
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
+ (id)atlasNamed:(id)_name {
    let a: id = msg![env; this alloc];
    let a: id = msg![env; a init];
    autorelease(env, a)
}
- (id)init { this }
- (id)textureNamed:(id)_name {
    msg_class![env; SKTexture textureWithImageNamed:_name]
}
- (id)textureNames { msg_class![env; NSArray array] }
@end

@implementation SKTransition: NSObject
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
- (id)init { this }
@end

@implementation MCPeerID: NSObject
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
- (id)initWithDisplayName:(id)_name { this }
- (id)displayName { get_static_str(env, "Player") }
@end

@implementation MCSession: NSObject
+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::new(crate::objc::TrivialHostObject), &mut env.mem)
}
- (id)initWithPeer:(id)_peer { this }
- (id)initWithPeer:(id)_peer securityIdentity:(id)_identity encryptionPreference:(i32)_pref { this }
- (id)connectedPeers { msg_class![env; NSArray array] }
- (id)delegate { nil }
- (())setDelegate:(id)_delegate { }
- (())disconnect { }
@end

};

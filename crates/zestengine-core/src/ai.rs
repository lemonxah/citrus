//! AI behaviour trees and perception: a data-driven behaviour-tree runtime
//! ticked against a shared blackboard, plus vision and hearing senses.

use std::collections::HashMap;

use glam::Vec3;
use serde::{Deserialize, Serialize};

/// A behaviour-tree node status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtStatus {
    Running,
    Success,
    Failure,
}

/// A behaviour-tree node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BtNode {
    Success,
    Failure,
    Selector { children: Vec<BtNode> },
    Sequence { children: Vec<BtNode> },
    Invert { child: Box<BtNode> },
    Condition { key: String, value: bool },
    Action { name: String },
}

/// Shared blackboard a tree reads each tick.
#[derive(Clone, Debug, Default)]
pub struct Blackboard {
    bools: HashMap<String, bool>,
    floats: HashMap<String, f32>,
    actions: HashMap<String, BtStatus>,
}

impl Blackboard {
    pub fn set_bool(&mut self, key: &str, value: bool) {
        self.bools.insert(key.to_owned(), value);
    }

    pub fn get_bool(&self, key: &str) -> bool {
        self.bools.get(key).copied().unwrap_or(false)
    }

    pub fn set_float(&mut self, key: &str, value: f32) {
        self.floats.insert(key.to_owned(), value);
    }

    pub fn get_float(&self, key: &str) -> f32 {
        self.floats.get(key).copied().unwrap_or(0.0)
    }

    pub fn set_action(&mut self, name: &str, status: BtStatus) {
        self.actions.insert(name.to_owned(), status);
    }
}

/// Evaluate `node` against the blackboard, returning its status.
pub fn tick(node: &BtNode, bb: &Blackboard) -> BtStatus {
    match node {
        BtNode::Success => BtStatus::Success,
        BtNode::Failure => BtStatus::Failure,
        BtNode::Selector { children } => {
            for c in children {
                match tick(c, bb) {
                    BtStatus::Success => return BtStatus::Success,
                    BtStatus::Running => return BtStatus::Running,
                    BtStatus::Failure => {}
                }
            }
            BtStatus::Failure
        }
        BtNode::Sequence { children } => {
            for c in children {
                match tick(c, bb) {
                    BtStatus::Failure => return BtStatus::Failure,
                    BtStatus::Running => return BtStatus::Running,
                    BtStatus::Success => {}
                }
            }
            BtStatus::Success
        }
        BtNode::Invert { child } => match tick(child, bb) {
            BtStatus::Success => BtStatus::Failure,
            BtStatus::Failure => BtStatus::Success,
            BtStatus::Running => BtStatus::Running,
        },
        BtNode::Condition { key, value } => {
            if bb.get_bool(key) == *value {
                BtStatus::Success
            } else {
                BtStatus::Failure
            }
        }
        BtNode::Action { name } => bb.actions.get(name).copied().unwrap_or(BtStatus::Failure),
    }
}

/// Vision and hearing senses for AI perception.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Perception {
    /// Vision distance.
    pub vision_range: f32,
    /// Vision half-angle (radians) about the forward axis.
    pub vision_angle: f32,
    /// Hearing distance (full sphere).
    pub hearing_range: f32,
}

impl Default for Perception {
    fn default() -> Self {
        Self {
            vision_range: 20.0,
            vision_angle: std::f32::consts::FRAC_PI_3,
            hearing_range: 15.0,
        }
    }
}

/// Whether `target` (world offset from the observer) is visible: within range,
/// inside the vision cone about `forward`, and `line_of_sight` is clear.
pub fn visible(perception: &Perception, forward: Vec3, target: Vec3, line_of_sight: bool) -> bool {
    let d = target.length();
    if d > perception.vision_range || !line_of_sight {
        return false;
    }
    if d <= 1e-4 {
        return true;
    }
    target.normalize().dot(forward.normalize_or(Vec3::Z)) >= perception.vision_angle.cos()
}

/// Whether `target` (world offset from the observer) is within hearing range.
pub fn audible(perception: &Perception, target: Vec3) -> bool {
    target.length() <= perception.hearing_range
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bb(bools: &[(&str, bool)]) -> Blackboard {
        let mut b = Blackboard::default();
        for (k, v) in bools {
            b.set_bool(k, *v);
        }
        b
    }

    #[test]
    fn selector_runs_until_success() {
        let tree = BtNode::Selector { children: vec![BtNode::Failure, BtNode::Success, BtNode::Failure] };
        assert_eq!(tick(&tree, &Blackboard::default()), BtStatus::Success);
    }

    #[test]
    fn sequence_fails_fast() {
        let tree = BtNode::Sequence { children: vec![BtNode::Success, BtNode::Failure, BtNode::Success] };
        assert_eq!(tick(&tree, &Blackboard::default()), BtStatus::Failure);
    }

    #[test]
    fn condition_and_invert() {
        let b = bb(&[("sees_player", true)]);
        let cond = BtNode::Condition { key: "sees_player".into(), value: true };
        assert_eq!(tick(&cond, &b), BtStatus::Success);
        assert_eq!(tick(&BtNode::Invert { child: Box::new(cond.clone()) }, &b), BtStatus::Failure);
    }

    #[test]
    fn action_reads_blackboard() {
        let mut b = Blackboard::default();
        b.set_action("shoot", BtStatus::Success);
        let tree = BtNode::Action { name: "shoot".into() };
        assert_eq!(tick(&tree, &b), BtStatus::Success);
        b.set_action("shoot", BtStatus::Failure);
        assert_eq!(tick(&tree, &b), BtStatus::Failure);
    }

    #[test]
    fn vision_needs_range_cone_and_los() {
        let p = Perception::default();
        let fwd = Vec3::Z;
        assert!(visible(&p, fwd, Vec3::new(0.0, 0.0, 5.0), true));
        assert!(!visible(&p, fwd, Vec3::new(0.0, 0.0, -5.0), true));
        assert!(!visible(&p, fwd, Vec3::new(0.0, 0.0, 100.0), true));
        assert!(!visible(&p, fwd, Vec3::new(0.0, 0.0, 5.0), false));
    }

    #[test]
    fn hearing_is_range_only() {
        let p = Perception::default();
        assert!(audible(&p, Vec3::new(10.0, 0.0, 0.0)));
        assert!(!audible(&p, Vec3::new(100.0, 0.0, 0.0)));
    }
}

#![allow(dead_code)]

extern crate rayon;
extern crate digest;
extern crate rand;
#[macro_use] extern crate proptest;

pub mod weight_unit;
pub mod message;
pub mod justification;
pub mod senders_weight;
pub mod traits;

mod example;


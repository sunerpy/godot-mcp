//! The operator's Android SDK and JDK, and what their tools answer.
//!
//! The server never downloads or installs any of them: an Android export and
//! a deploy use exactly the SDK and JDK the operator configured, and answer
//! `unavailable` naming what is missing otherwise.
//!
//! | module | responsibility |
//! | --- | --- |
//! | [`sdk`] | finding `adb`, `apksigner`, `keytool` and `java` inside the configured directories |
//! | [`tool`] | running one of them with an exact environment, a deadline and bounded output |
//! | [`adb`] | reading what `adb` answers: the device list, an install, a launch, a process |

pub mod adb;
pub mod sdk;
pub mod tool;

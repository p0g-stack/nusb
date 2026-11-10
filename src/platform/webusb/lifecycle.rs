//! A JS-object lease is shared even by independent nusb Device instances (and
//! wasm modules). State-changing promises are serialized so cancellation cannot
//! close before a pending open/claim finishes. See upstream PR #221.
use js_sys::{Function, Object, Promise, Reflect};
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::UsbDevice;

use crate::Error;
use super::js_value_to_error;

const STATE_KEY: &str = "__nusb_open_state_v1";

pub(crate) struct OpenLease {
    device: UsbDevice,
    state: Object,
    ready: Promise,
}

fn users(state: &Object) -> f64 {
    Reflect::get(state, &"users".into()).unwrap().as_f64().unwrap()
}

fn set_users(state: &Object, value: f64) {
    Reflect::set(state, &"users".into(), &value.into()).unwrap();
}

// Both settlement paths run the next action: a failed claim must not prevent
// cleanup or poison a later independent open. The returned promise still carries
// the current action's rejection to its caller.
fn enqueue(state: &Object, action: impl FnOnce() -> JsValue + 'static) -> Promise {
    let tail = Reflect::get(state, &"tail".into()).unwrap();
    let then: Function = Reflect::get(&tail, &"then".into()).unwrap().unchecked_into();
    let callback = Closure::once_into_js(move |_: JsValue| action());
    let next: Promise = then.call2(&tail, &callback, &callback).unwrap().unchecked_into();
    Reflect::set(state, &"tail".into(), &next).unwrap();
    next
}

impl OpenLease {
    pub(crate) fn acquire(device: UsbDevice) -> Result<Self, Error> {
        let existing = Reflect::get(&device, &STATE_KEY.into()).map_err(js_value_to_error)?;
        let state = if existing.is_undefined() {
            let state = Object::new();
            set_users(&state, 0.0);
            Reflect::set(&state, &"tail".into(), &Promise::resolve(&JsValue::UNDEFINED))
                .map_err(js_value_to_error)?;
            Reflect::set(&device, &STATE_KEY.into(), &state).map_err(js_value_to_error)?;
            state
        } else {
            existing.dyn_into::<Object>().map_err(js_value_to_error)?
        };
        set_users(&state, users(&state) + 1.0);
        let handle = device.clone();
        let ready = enqueue(&state, move || {
            if handle.opened() { JsValue::UNDEFINED } else { handle.open().into() }
        });
        Ok(Self { device, state, ready })
    }

    pub(crate) fn ready(&self) -> Promise { self.ready.clone() }

    pub(crate) fn change(&self, action: impl FnOnce() -> JsValue + 'static) -> Promise {
        enqueue(&self.state, action)
    }

    pub(crate) fn force_close(&self) -> Promise {
        let device = self.device.clone();
        self.change(move || device.close().into())
    }
}

impl Drop for OpenLease {
    fn drop(&mut self) {
        let count = users(&self.state) - 1.0;
        set_users(&self.state, count);
        if count == 0.0 {
            let device = self.device.clone();
            let state = self.state.clone();
            let promise = enqueue(&self.state, move || {
                if users(&state) == 0.0 && device.opened() {
                    device.close().into()
                } else { JsValue::UNDEFINED }
            });
            spawn_local(async move {
                if let Err(error) = JsFuture::from(promise).await {
                    log::debug!("WebUSB close on drop: {}", js_value_to_error(error));
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    fn device() -> UsbDevice {
        js_sys::eval(r#"({
            opened:false, opens:0, closes:0,
            open() { this.opens++; this.opened=true; return Promise.resolve(); },
            close() { this.closes++; this.opened=false; return Promise.resolve(); }
        })"#).unwrap().unchecked_into()
    }

    async fn drain(state: &Object) {
        let tail: Promise = Reflect::get(state, &"tail".into()).unwrap().unchecked_into();
        JsFuture::from(tail).await.unwrap();
    }

    fn count(device: &UsbDevice, key: &str) -> f64 {
        Reflect::get(device, &key.into()).unwrap().as_f64().unwrap()
    }

    #[wasm_bindgen_test]
    async fn independent_handles_close_only_after_last_lease() {
        let device = device();
        let first = OpenLease::acquire(device.clone()).unwrap();
        let second = OpenLease::acquire(device.clone()).unwrap();
        JsFuture::from(second.ready()).await.unwrap();
        let state = second.state.clone();
        assert_eq!(count(&device, "opens"), 1.0);
        drop(first);
        drain(&state).await;
        assert_eq!(count(&device, "closes"), 0.0);
        drop(second);
        drain(&state).await;
        assert_eq!(count(&device, "closes"), 1.0);
        assert_eq!(users(&state), 0.0);
    }

    #[wasm_bindgen_test]
    async fn cancellation_before_open_settles_still_closes() {
        let device = device();
        let lease = OpenLease::acquire(device.clone()).unwrap();
        let state = lease.state.clone();
        drop(lease);
        drain(&state).await;
        assert_eq!(count(&device, "opens"), 1.0);
        assert_eq!(count(&device, "closes"), 1.0);
    }

    #[wasm_bindgen_test]
    async fn failed_state_change_does_not_prevent_force_close_or_reopen() {
        let device = device();
        let lease = OpenLease::acquire(device.clone()).unwrap();
        JsFuture::from(lease.ready()).await.unwrap();
        let failed = lease.change(|| Promise::reject(&"claim rejected".into()).into());
        assert!(JsFuture::from(failed).await.is_err());
        JsFuture::from(lease.force_close()).await.unwrap();
        assert_eq!(count(&device, "closes"), 1.0);
        let other = OpenLease::acquire(device.clone()).unwrap();
        JsFuture::from(other.ready()).await.unwrap();
        assert_eq!(count(&device, "opens"), 2.0);
    }
}

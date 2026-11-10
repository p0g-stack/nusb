use wasm_bindgen_futures::JsFuture;
use web_sys::{UsbDevice, UsbDeviceFilter, UsbDeviceRequestOptions};

use crate::{
    enumeration::{DeviceSelector, StringDescriptor},
    DeviceInfo, Error, InterfaceInfo, MaybeFuture,
};

use super::{js_value_to_error, WebFuture};

pub fn list_devices() -> impl MaybeFuture<Output = Result<impl Iterator<Item = DeviceInfo>, Error>>
{
    WebFuture(async move {
        let usb = super::usb()?;
        let devices = JsFuture::from(usb.get_devices())
            .await
            .map_err(js_value_to_error)?;
        Ok(devices.into_iter().map(device_to_info))
    })
}

pub(crate) fn device_to_info(device: UsbDevice) -> DeviceInfo {
    DeviceInfo {
        vendor_id: device.vendor_id(),
        product_id: device.product_id(),
        device_version: ((device.device_version_major() as u16) << 8)
            | ((device.device_version_minor() as u16) << 4)
            | (device.device_version_subminor() as u16),
        usb_version: ((device.usb_version_major() as u16) << 8)
            | ((device.usb_version_minor() as u16) << 4)
            | (device.usb_version_subminor() as u16),
        class: device.device_class(),
        subclass: device.device_subclass(),
        protocol: device.device_protocol(),
        manufacturer_string: device
            .manufacturer_name()
            .map_or(StringDescriptor::NotPresent, StringDescriptor::Cached),
        product_string: device
            .product_name()
            .map_or(StringDescriptor::NotPresent, StringDescriptor::Cached),
        serial_number: device
            .serial_number()
            .map_or(StringDescriptor::NotPresent, StringDescriptor::Cached),
        interfaces: if let Some(config) = device.configuration() {
            config
                .interfaces()
                .into_iter()
                .filter_map(|iface| {
                    let alt = iface.alternates().iter().next()?;
                    Some(InterfaceInfo {
                        interface_number: iface.interface_number(),
                        class: alt.interface_class(),
                        subclass: alt.interface_subclass(),
                        protocol: alt.interface_protocol(),
                        interface_string: alt
                            .interface_name()
                            .map_or(StringDescriptor::NotPresent, StringDescriptor::Cached),
                    })
                })
                .collect()
        } else {
            vec![]
        },
        device: device.clone(),
    }
}

pub fn request_device(
    selectors: &[DeviceSelector],
) -> impl MaybeFuture<Output = Result<Option<DeviceInfo>, Error>> + use<'_> {
    let filters = selector_to_filters(selectors);
    WebFuture(async move {
        let usb = super::usb()?;
        let device = if filters.is_empty() {
            // WebUSB treats an empty filter list as matching all devices, but
            // we'll go with the behavior of `.iter().any()` that makes more sense.
            None
        } else {
            match JsFuture::from(usb.request_device(&UsbDeviceRequestOptions::new(&filters))).await {
                Ok(device) => Some(device_to_info(device)),
                Err(error) => return cancelled_or_error(error),
            }
        };

        Ok(device)
    })
}

// Only dismissal of the chooser is cancellation. Permission policy, transient
// activation and browser permission failures must remain actionable errors.
fn cancelled_or_error(
    error: wasm_bindgen::JsValue,
) -> Result<Option<DeviceInfo>, Error> {
    if super::rejection_field(&error, "name").as_deref() == Some("NotFoundError") {
        Ok(None)
    } else {
        Err(js_value_to_error(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn only_not_found_is_cancellation() {
        for name in ["NotFoundError", "SecurityError", "NotAllowedError", "InvalidStateError"] {
            let error = web_sys::js_sys::Object::new();
            web_sys::js_sys::Reflect::set(&error, &"name".into(), &name.into()).unwrap();
            web_sys::js_sys::Reflect::set(
                &error,
                &"message".into(),
                &"requestDevice requires transient activation".into(),
            ).unwrap();
            let result = cancelled_or_error(error.into());
            if name == "NotFoundError" {
                assert!(matches!(result, Ok(None)));
            } else {
                let error = result.unwrap_err().to_string();
                assert!(error.contains(name));
                assert!(error.contains("requestDevice requires transient activation"));
            }
        }
    }

    #[wasm_bindgen_test]
    fn unstructured_rejection_is_not_cancellation() {
        assert!(cancelled_or_error("transient activation required".into()).is_err());
        assert!(cancelled_or_error(wasm_bindgen::JsValue::UNDEFINED).is_err());
    }
}

fn selector_to_filters(selectors: &[DeviceSelector]) -> Vec<UsbDeviceFilter> {
    selectors
        .iter()
        .map(|rule| {
            let filter = UsbDeviceFilter::new();
            let DeviceSelector {
                vendor_id,
                product_id,
                class,
                subclass,
                protocol,
                ref serial_number,
            } = *rule;

            if let Some(vendor_id) = vendor_id {
                filter.set_vendor_id(vendor_id);
            }
            if let Some(product_id) = product_id {
                filter.set_product_id(product_id);
            }
            if let Some(class) = class {
                filter.set_class_code(class);
            }
            if let Some(subclass) = subclass {
                filter.set_subclass_code(subclass);
            }
            if let Some(protocol) = protocol {
                filter.set_protocol_code(protocol);
            }
            if let Some(ref serial_number) = serial_number {
                filter.set_serial_number(serial_number);
            }
            filter
        })
        .collect::<Vec<_>>()
}

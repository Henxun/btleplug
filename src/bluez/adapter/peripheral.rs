// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.
//
// Some portions of this file are taken and/or modified from Rumble
// (https://github.com/mwylde/rumble), using a dual MIT/Apache License under the
// following copyright:
//
// Copyright (c) 2014 The Rust Project Developers

use dbus::{
    arg::{RefArg, Variant},
    blocking::{stdintf::org_freedesktop_dbus::PropertiesPropertiesChanged, Proxy, SyncConnection},
    channel::Token,
    message::{Message, SignalArgs},
    Path,
};

use bytes::BufMut;

use crate::{
    api::{
        AdapterManager, AddressType, BDAddr, CharPropFlags, Characteristic, CommandCallback,
        NotificationHandler, Peripheral as ApiPeripheral, PeripheralProperties, RequestCallback,
        UUID,
    },
    bluez::{bluez_dbus::device::OrgBluezDevice1, AttributeType, Handle, BLUEZ_DEST},
    Error, Result,
};

use std::{
    collections::{BTreeSet, HashMap},
    fmt::{self, Debug, Display, Formatter},
    sync::{
        mpsc::{Receiver, Sender},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, PartialOrd)]
enum PeripheralState {
    NotConnected,
    Connected,
    ServicesResolved,
}

#[derive(Clone)]
pub struct Peripheral {
    adapter: AdapterManager<Self>,
    connection: Arc<SyncConnection>,
    path: String,
    address: BDAddr,
    properties: Arc<Mutex<PeripheralProperties>>,
    characteristics: Arc<Mutex<BTreeSet<Characteristic>>>,
    attributes_map: Arc<Mutex<HashMap<u16, (String, Handle, Characteristic)>>>,
    state: Arc<(Mutex<PeripheralState>, Condvar)>,
    notification_handlers: Arc<Mutex<Vec<NotificationHandler>>>,
    listen_token: Arc<Mutex<Option<Token>>>,
}

impl Peripheral {
    pub fn new(
        adapter: AdapterManager<Self>,
        connection: Arc<SyncConnection>,
        path: &str,
        address: BDAddr,
    ) -> Self {
        let mut properties = PeripheralProperties::default();
        properties.address = address;
        let properties = Arc::new(Mutex::new(properties));
        let characteristics = Arc::new(Mutex::new(BTreeSet::new()));
        let notification_handlers = Arc::new(Mutex::new(Vec::new()));

        Peripheral {
            adapter: adapter,
            connection: connection,
            path: path.to_string(),
            address: address,
            state: Arc::new((Mutex::new(PeripheralState::NotConnected), Condvar::new())),
            properties: properties,
            attributes_map: Arc::new(Mutex::new(HashMap::new())),
            characteristics: characteristics,
            notification_handlers: notification_handlers,
            listen_token: Arc::new(Mutex::new(None)),
        }
    }

    pub fn properties_changed(
        &self,
        args: PropertiesPropertiesChanged,
        _connection: &SyncConnection,
        message: &Message,
    ) -> bool {
        let path = message.path().unwrap().into_static();
        let path = path.as_str().unwrap();
        if path.starts_with(self.path.as_str()) {
            if let Ok(_handle) = path.parse::<Handle>() {
                warn!("TODO: Support for handling properties changed on an attribute");
            } else {
                self.update_properties(&args.changed_properties);
                if !args.invalidated_properties.is_empty() {
                    warn!(
                        "TODO: Got some properties to invalidate!\n\t{:?}",
                        args.invalidated_properties
                    );
                }
            }
        } else {
            error!(
                "Got properties changed for {}, but does not start with {}",
                path, self.path
            );
        }

        true
    }

    pub fn listen(&mut self, listener: &SyncConnection) -> Result<()> {
        let peripheral = self.clone();
        let mut rule = PropertiesPropertiesChanged::match_rule(None, None);
        // For some silly lifetime reasons, we need to assign path separately... 
        rule.path = Some(Path::from(self.path.clone()));
        // And also, we're interested in properties changed on all sub elements
        rule.path_is_namespace = true;
        let token =
            listener.add_match(rule, move |a, s, m| peripheral.properties_changed(a, s, m))?;
        *self.listen_token.lock().unwrap() = Some(token);

        Ok(())
    }

    pub fn stop_listening(&mut self, listener: &SyncConnection) -> Result<()> {
        let mut token = self.listen_token.lock().unwrap();
        if token.is_some() {
            listener.remove_match(token.unwrap())?;
            *token = None;
        }

        Ok(())
    }

    pub fn add_attribute(&self, path: &str, uuid: UUID, properties: CharPropFlags) -> Result<()> {
        trace!(
            "Adding attribute {} ({:?}) under {}",
            uuid,
            properties,
            path
        );
        let mut path_uuid_map = self.attributes_map.lock().unwrap();
        let handle: Handle = path.parse()?;
        // Create a placeholder attribute to store properties and uuid
        let attribute = Characteristic {
            uuid: uuid,
            value_handle: handle.handle,
            properties: properties,
            end_handle: 0,
            start_handle: 0,
        };

        let result = path_uuid_map.insert(handle.handle, (path.to_string(), handle, attribute));
        if let Some((_old_path, old_handle, _old_characteristic)) = result {
            error!(
                "Found (and replaced) existing DBus characteristic mapping!\n\tOld: {} -> {}\n\tNew: {} -> {}",
                old_handle.handle, path,
                handle.handle, path,
            )
        }

        Ok(())
    }

    fn build_characteristic_ranges(&self) -> Result<()> {
        let handles = self.attributes_map.lock().unwrap();

        let mut services = handles
            .iter()
            .filter(|(_k, (_p, h, _v))| h.typ == AttributeType::Service)
            .map(|(_h, (_p, k, v))| (k, v))
            .peekable();

        let mut result = self.characteristics.lock().unwrap();
        result.clear();

        // TODO: Verify that service attributes are returned as characteristics
        while let Some((handle, attribute)) = services.next() {
            let next = services.peek();
            result.insert(Characteristic {
                start_handle: handle.handle,
                end_handle: next.map_or(u16::MAX, |n| n.0.handle - 1),
                value_handle: handle.handle,
                properties: attribute.properties,
                uuid: attribute.uuid.clone(),
            });
        }

        let mut characteristics = handles
            .iter()
            .filter(|(_h, (_p, k, _v))| k.typ == AttributeType::Characteristic)
            .map(|(_h, (_p, k, v))| (k, v))
            .peekable();

        while let Some((handle, attribute)) = characteristics.next() {
            let next = characteristics.peek();
            result.insert(Characteristic {
                start_handle: handle.handle,
                end_handle: next.map_or(u16::MAX, |n| n.0.handle - 1),
                value_handle: handle.handle,
                properties: attribute.properties,
                uuid: attribute.uuid.clone(),
            });
        }

        Ok(())
    }

    pub fn update_properties(
        &self,
        args: &::std::collections::HashMap<String, Variant<Box<dyn RefArg + 'static>>>,
    ) {
        trace!("Updating peripheral properties");
        let mut properties = self.properties.lock().unwrap();

        properties.discovery_count += 1;

        if let Some(connected) = args.get("Connected") {
            let connected = connected.0.as_u64().unwrap() > 0;
            debug!(
                "Updating \"{}\" connected to \"{:?}\"",
                self.address, connected
            );
            let (ref lock, ref cvar) = *self.state;
            let mut state = lock.lock().unwrap();
            if connected && *state == PeripheralState::NotConnected {
                *state = PeripheralState::Connected;
            } else if !connected {
                *state = PeripheralState::NotConnected;
            }
            cvar.notify_all();
        }

        if let Some(name) = args.get("Name") {
            debug!("Updating \"{}\" local name to \"{:?}\"", self.address, name);
            properties.local_name = name.as_str().map(|s| s.to_string());
        }

        if let Some(services_resolved) = args.get("ServicesResolved") {
            let services_resolved = services_resolved.0.as_u64().unwrap() > 0;
            if services_resolved {
                // Need to prase and figure out handle ranges for all discovered characteristics.
                self.build_characteristic_ranges().unwrap();
            }
            // All services have been discovered, time to inform anyone waiting.
            let (ref lock, ref cvar) = *self.state;
            if services_resolved {
                *lock.lock().unwrap() = PeripheralState::ServicesResolved;
            }
            cvar.notify_all();
        }

        // if let Some(services) = args.get("ServiceData") {
        //     debug!("Updating services to \"{:?}\"", services);

        //     if let Some(mut iter) = services.0.as_iter() {
        //         loop {
        //             if let (Some(uuid), ())
        //         }
        //     }
        // }

        // As of writing this: ManufacturerData returns a 'Variant({<manufacturer_id>: Variant([<manufacturer_data>])})'.
        // This Variant wrapped dictionary and array is difficult to navigate. So uh.. trust me, this works on my machine™.
        if let Some(manufacturer_data) = args.get("ManufacturerData") {
            debug!(
                "Updating \"{}\" manufacturer data \"{:?}\"",
                self.address, manufacturer_data
            );
            let mut result = Vec::<u8>::new();
            // dbus-rs doesn't really have a dictionary API... so need to iterate two at a time and make a key-value pair.
            if let Some(mut iter) = manufacturer_data.0.as_iter() {
                loop {
                    if let (Some(id), Some(data)) = (iter.next(), iter.next()) {
                        // This API is terrible.. why can't I just get an array out, why is it all wrapped in a Variant?
                        let data: Vec<u8> = data
                            .as_iter() // 🎶 The Variant is connected to the
                            .unwrap() // Array type!
                            .next() // The Array type is connected to the
                            .unwrap() // Array of integers!
                            .as_iter() // Lets convert the
                            .unwrap() // integers to a
                            .map(|b| b.as_u64().unwrap() as u8) // array of bytes...
                            .collect(); // I got too lazy to make it rhyme... 🎶

                        result.put_u16_le(id.as_u64().map(|v| v as u16).unwrap());
                        result.extend(data);
                    } else {
                        break;
                    }
                }
            }
            // 🎉
            properties.manufacturer_data = Some(result);
        }

        if let Some(address_type) = args.get("AddressType") {
            debug!(
                "Updating \"{}\" address type \"{:?}\"",
                self.address, address_type
            );
            properties.address_type = address_type
                .as_str()
                .map(|address_type| AddressType::from_str(address_type).unwrap_or_default())
                .unwrap_or_default();
        }

        if let Some(rssi) = args.get("RSSI") {
            debug!("Updating \"{}\" RSSI \"{:?}\"", self.address, rssi);
            properties.tx_power_level = rssi.as_i64().map(|rssi| rssi as i8);
        }
    }

    pub fn proxy(&self) -> Proxy<&SyncConnection> {
        self.connection
            .with_proxy(BLUEZ_DEST, &self.path, Duration::from_secs(30))
    }

    pub fn proxy_for(&self, characteristic: &Characteristic) -> Option<Proxy<&SyncConnection>> {
        let map = self.attributes_map.lock().unwrap();
        map.get(&characteristic.value_handle).map(|(path, _h, _c)| {
            self.connection
                .with_proxy(BLUEZ_DEST, path.clone(), Duration::from_secs(30))
        })
    }
}

assert_impl_all!(Peripheral: Sync, Send);

impl Display for Peripheral {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let connected = if self.is_connected() {
            " connected"
        } else {
            ""
        };
        let properties = self.properties.lock().unwrap();
        write!(
            f,
            "{} {}{}",
            self.address,
            properties
                .local_name
                .clone()
                .unwrap_or("(unknown)".to_string()),
            connected
        )
    }
}

impl Debug for Peripheral {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let connected = if self.is_connected() {
            " connected"
        } else {
            ""
        };
        let properties = self.properties.lock().unwrap();
        let characteristics = self.characteristics.lock().unwrap();
        write!(
            f,
            "{} properties: {:?}, characteristics: {:?} {}",
            self.address, *properties, *characteristics, connected
        )
    }
}

impl ApiPeripheral for Peripheral {
    fn address(&self) -> BDAddr {
        self.address.clone()
    }

    fn properties(&self) -> PeripheralProperties {
        let l = self.properties.lock().unwrap();
        l.clone()
    }

    fn characteristics(&self) -> BTreeSet<Characteristic> {
        let l = self.characteristics.lock().unwrap();
        l.clone()
    }

    fn is_connected(&self) -> bool {
        *self.state.0.lock().unwrap() >= PeripheralState::Connected
    }

    fn connect(&self) -> Result<()> {
        let started = Instant::now();
        if let Err(error) = self.proxy().connect() {
            match error.name() {
                Some("org.bluez.Error.AlreadyConnected") => Ok(()),
                Some("org.bluez.Error.Failed") => {
                    error!(
                        "BlueZ Failed to connect to \"{:?}\": {}",
                        self.address,
                        error.message().unwrap()
                    );
                    Err(Error::NotConnected)
                }
                _ => Err(error)?,
            }
        } else {
            Ok(())
        }?;
        // For somereason, BlueZ may return an Okay result before the the device is actually connected...
        // So lets wait for the "connected" property to update to true
        let (ref lock, ref cvar) = *self.state;
        let timeout = Duration::from_secs(30) - Instant::now().duration_since(started);
        // Map the result of the wait_timeout_while(...) to match our Result<()>
        cvar.wait_timeout_while(lock.lock().unwrap(), timeout, |c| {
            *c < PeripheralState::Connected
        })
        .map_err(|_| Error::TimedOut(Duration::from_secs(30)))
        .map(|_| ())
    }

    fn disconnect(&self) -> Result<()> {
        Ok(self.proxy().disconnect()?)
    }

    fn discover_characteristics(&self) -> Result<Vec<Characteristic>> {
        self.discover_characteristics_in_range(0x0001, 0xFFFF)
    }

    fn discover_characteristics_in_range(
        &self,
        _start: u16,
        _end: u16,
    ) -> Result<Vec<Characteristic>> {
        let (ref lock, ref cvar) = *self.state;
        trace!("Waiting for all services to be resolved");
        let _guard = cvar
            .wait_while(lock.lock().unwrap(), |b| *b == PeripheralState::Connected)
            .unwrap();

        if *_guard == PeripheralState::NotConnected {
            return Err(Error::NotConnected);
        }

        debug!("All services are now resolved!");

        Ok(self
            .characteristics
            .lock()
            .unwrap()
            .clone()
            .into_iter()
            .filter(|c| c.value_handle >= _start && c.value_handle <= _end)
            .collect())
    }

    fn command_async(
        &self,
        _characteristic: &Characteristic,
        _data: &[u8],
        _handler: Option<CommandCallback>,
    ) {
        unimplemented!()
    }

    fn command(&self, characteristic: &Characteristic, data: &[u8]) -> Result<()> {
        use crate::bluez::bluez_dbus::gatt_characteristic::OrgBluezGattCharacteristic1;
        Ok(self
            .proxy_for(&characteristic)
            .map(|p| p.write_value(Vec::from(data), HashMap::new()))
            .ok_or(Error::NotSupported("write_without_response".to_string()))??)
    }

    fn request_async(
        &self,
        _characteristic: &Characteristic,
        _data: &[u8],
        _handler: Option<RequestCallback>,
    ) {
        unimplemented!()
    }

    fn request(&self, characteristic: &Characteristic, data: &[u8]) -> Result<Vec<u8>> {
        self.command(characteristic, data)?;

        self.read(characteristic)
    }

    fn read_async(&self, _characteristic: &Characteristic, _handler: Option<RequestCallback>) {
        unimplemented!()
    }

    fn read(&self, characteristic: &Characteristic) -> Result<Vec<u8>> {
        use crate::bluez::bluez_dbus::gatt_characteristic::OrgBluezGattCharacteristic1;
        Ok(self
            .proxy_for(&characteristic)
            .map(|p| p.read_value(HashMap::new()))
            .ok_or(Error::NotSupported("read".to_string()))??)
    }

    fn read_by_type_async(
        &self,
        _characteristic: &Characteristic,
        _uuid: UUID,
        _handler: Option<RequestCallback>,
    ) {
        unimplemented!()
    }

    // Is this looking for a characteristic with a descriptor? or a service with a characteristic?
    fn read_by_type(&self, characteristic: &Characteristic, uuid: UUID) -> Result<Vec<u8>> {
        if let Some(characteristic) =
            self.attributes_map
                .lock()
                .unwrap()
                .iter()
                .find_map(|(_k, (_p, _h, c))| {
                    if c.uuid == uuid
                        && c.value_handle >= characteristic.start_handle
                        && c.value_handle <= characteristic.end_handle
                    {
                        Some(c)
                    } else {
                        None
                    }
                })
        {
            self.read(characteristic)
        } else {
            Err(Error::NotSupported("read_by_type".to_string()))
        }
    }

    fn subscribe(&self, _characteristic: &Characteristic) -> Result<()> {
        unimplemented!()
    }

    fn unsubscribe(&self, _characteristic: &Characteristic) -> Result<()> {
        unimplemented!()
    }

    fn on_notification(&self, _handler: NotificationHandler) {
        unimplemented!()
    }
}

// Copyright 2024 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use std::{collections::BTreeSet, sync::Arc};

use anyhow::Context;
use cosmic::{
    iced::{alignment, Length},
    iced_core::text::Wrap,
    prelude::CollectionWidget,
    widget, Apply, Command, Element,
};
use cosmic_settings_page::{self as page, section, Section};
use cosmic_settings_subscriptions::network_manager::{
    self, current_networks::ActiveConnectionInfo, NetworkManagerState,
};
use slab::Slab;

pub type ConnectionId = Arc<str>;

#[derive(Clone, Debug)]
pub enum Message {
    /// Activate a connection
    Activate(ConnectionId),
    /// Add a network connection with nm-connection-editor
    AddNetwork,
    /// Deactivate a connection.
    Deactivate(ConnectionId),
    /// An error occurred.
    Error(String),
    /// An update from the network manager daemon
    NetworkManager(network_manager::Event),
    /// Successfully connected to the system dbus.
    NetworkManagerConnect(
        (
            zbus::Connection,
            tokio::sync::mpsc::Sender<crate::pages::Message>,
        ),
    ),
    /// Refresh devices and their connection profiles
    Refresh,
    /// Remove a connection profile
    RemoveProfile(ConnectionId),
    /// Opens settings page for the access point.
    Settings(ConnectionId),
    /// Update NetworkManagerState
    UpdateState(NetworkManagerState),
    /// Update the devices lists
    UpdateDevices(Vec<network_manager::devices::DeviceInfo>),
    /// Display more options for an access point
    ViewMore(Option<ConnectionId>),
}

pub type InterfaceId = String;

#[derive(Debug, Default)]
pub struct Page {
    nm_task: Option<tokio::sync::oneshot::Sender<()>>,
    nm_state: Option<NmState>,
    view_more_popup: Option<ConnectionId>,
    // connections: IndexMap<ConnectionId, InterfaceId>,
    connecting: BTreeSet<ConnectionId>,
}

#[derive(Debug)]
pub struct NmState {
    conn: zbus::Connection,
    sender: futures::channel::mpsc::UnboundedSender<network_manager::Request>,
    active_conns: Vec<ActiveConnectionInfo>,
    devices: Vec<network_manager::devices::DeviceInfo>,
}

impl page::AutoBind<crate::pages::Message> for Page {}

impl page::Page<crate::pages::Message> for Page {
    fn info(&self) -> cosmic_settings_page::Info {
        page::Info::new("wired", "preferences-network-and-wireless-symbolic")
            .title(fl!("wired"))
            .description(fl!("connections-and-profiles", variant = "wired"))
    }

    fn content(
        &self,
        sections: &mut slotmap::SlotMap<section::Entity, Section<crate::pages::Message>>,
    ) -> Option<page::Content> {
        Some(vec![sections.insert(devices_view())])
    }

    fn header_view(&self) -> Option<cosmic::Element<'_, crate::pages::Message>> {
        Some(
            widget::button::standard(fl!("add-network"))
                .on_press(Message::AddNetwork)
                .apply(widget::container)
                .width(Length::Fill)
                .align_x(alignment::Horizontal::Right)
                .apply(Element::from)
                .map(crate::pages::Message::Wired),
        )
    }

    fn on_enter(
        &mut self,
        _page: cosmic_settings_page::Entity,
        sender: tokio::sync::mpsc::Sender<crate::pages::Message>,
    ) -> cosmic::Command<crate::pages::Message> {
        if self.nm_task.is_none() {
            return cosmic::command::future(async move {
                zbus::Connection::system()
                    .await
                    .context("failed to create system dbus connection")
                    .map_or_else(
                        |why| Message::Error(why.to_string()),
                        |conn| Message::NetworkManagerConnect((conn, sender.clone())),
                    )
                    .apply(crate::pages::Message::Wired)
            });
        }

        Command::none()
    }

    fn on_leave(&mut self) -> Command<crate::pages::Message> {
        self.view_more_popup = None;
        self.nm_state = None;
        self.connecting.clear();
        if let Some(cancel) = self.nm_task.take() {
            _ = cancel.send(());
        }

        Command::none()
    }
}

impl Page {
    pub fn update(&mut self, message: Message) -> Command<crate::app::Message> {
        match message {
            Message::NetworkManager(network_manager::Event::RequestResponse {
                req,
                state,
                success,
            }) => {
                if !success {
                    tracing::error!(request = ?req, "network-manager request failed");
                }

                if let Some(NmState {
                    ref conn,
                    ref mut active_conns,
                    ..
                }) = self.nm_state
                {
                    *active_conns = state
                        .active_conns
                        .into_iter()
                        .filter(|info| matches!(info, ActiveConnectionInfo::Wired { .. }))
                        .collect();

                    return update_devices(conn.clone());
                }
            }

            Message::UpdateDevices(devices) => {
                if let Some(ref mut nm_state) = self.nm_state {
                    nm_state.devices = devices;
                }
            }

            Message::UpdateState(state) => {
                if let Some(ref mut nm_state) = self.nm_state {
                    nm_state.active_conns = state
                        .active_conns
                        .into_iter()
                        .filter(|info| matches!(info, ActiveConnectionInfo::Wired { .. }))
                        .collect();
                }
            }

            Message::NetworkManager(
                network_manager::Event::ActiveConns | network_manager::Event::Devices,
            ) => {
                if let Some(NmState { ref conn, .. }) = self.nm_state {
                    return cosmic::command::batch(vec![
                        update_state(conn.clone()),
                        update_devices(conn.clone()),
                    ]);
                }
            }

            Message::NetworkManager(network_manager::Event::Init {
                conn,
                sender,
                state,
            }) => {
                self.nm_state = Some(NmState {
                    conn: conn.clone(),
                    sender,
                    devices: Vec::new(),
                    active_conns: state
                        .active_conns
                        .into_iter()
                        .filter(|info| matches!(info, ActiveConnectionInfo::Wired { .. }))
                        .collect(),
                });

                return update_devices(conn);
            }

            Message::NetworkManager(event) => (),

            Message::AddNetwork => {
                tokio::task::spawn(super::nm_add_wired());
            }

            Message::Activate(uuid) => {
                self.view_more_popup = None;
                if let Some(NmState {
                    ref devices,
                    ref sender,
                    ..
                }) = self.nm_state
                {
                    if let Some(device) = devices.get(0) {
                        let device_conn = device
                            .available_connections
                            .iter()
                            .find(|conn| conn.uuid.as_ref() == uuid.as_ref());

                        if let Some(device_conn) = device_conn {
                            let device_path = device.path.clone();
                            let conn_path = device_conn.path.clone();

                            _ = sender.unbounded_send(network_manager::Request::Activate(
                                device_path,
                                conn_path,
                            ));
                        }
                    }
                }
            }

            Message::Deactivate(uuid) => {
                self.view_more_popup = None;
                if let Some(NmState { ref sender, .. }) = self.nm_state {
                    _ = sender.unbounded_send(network_manager::Request::Deactivate(uuid));
                }
            }

            Message::RemoveProfile(uuid) => {
                self.view_more_popup = None;
                // TODO: Call delete method on a ConnectionSettings object.
            }

            Message::ViewMore(uuid) => {
                self.view_more_popup = uuid;
            }

            Message::Settings(uuid) => {
                self.view_more_popup = None;

                return cosmic::command::future(async move {
                    _ = super::nm_edit_connection(uuid.as_ref()).await;
                    // TODO: Update when iced is rebased to use then method.
                    Message::Refresh
                })
                .map(crate::pages::Message::Wired)
                .map(crate::app::Message::PageMessage);
            }

            Message::Refresh => {
                if let Some(NmState { ref conn, .. }) = self.nm_state {
                    return cosmic::command::batch(vec![
                        update_state(conn.clone()),
                        update_devices(conn.clone()),
                    ]);
                }
            }

            Message::Error(why) => {
                tracing::error!(why, "error in wired settings page");
            }

            Message::NetworkManagerConnect((conn, output)) => {
                self.connect(conn.clone(), output);
            }
        }

        Command::none()
    }

    fn connect(
        &mut self,
        conn: zbus::Connection,
        sender: tokio::sync::mpsc::Sender<crate::pages::Message>,
    ) {
        if self.nm_task.is_none() {
            self.nm_task = Some(crate::utils::forward_event_loop(
                sender,
                |event| crate::pages::Message::Wired(Message::NetworkManager(event)),
                move |tx| async move {
                    futures::join!(
                        network_manager::watch(conn.clone(), tx.clone()),
                        network_manager::active_conns::watch(conn.clone(), tx.clone()),
                        network_manager::devices::watch(conn, true, tx)
                    );
                },
            ));
        }
    }
}

fn devices_view() -> Section<crate::pages::Message> {
    crate::slab!(descriptions {
        wired_conns_txt = fl!("wired", "connections");
        remove_txt = fl!("wired", "remove");
        connect_txt = fl!("connect");
        connected_txt = fl!("connected");
        settings_txt = fl!("settings");
        disconnect_txt = fl!("disconnect");
    });

    Section::default()
        .descriptions(descriptions)
        .view::<Page>(move |_binder, page, section| {
            let Some(NmState {
                ref active_conns,
                ref devices,
                ..
            }) = page.nm_state
            else {
                return cosmic::widget::column().into();
            };

            let theme = cosmic::theme::active();
            let spacing = &theme.cosmic().spacing;

            let mut view = widget::column::with_capacity(4);

            for device in devices {
                let has_multiple_connection_profiles = device.available_connections.len() > 1;
                let header_txt = format!(
                    "{} ({})",
                    section.descriptions[wired_conns_txt], device.interface
                );
                let known_networks = device.available_connections.iter().fold(
                    widget::settings::view_section(header_txt),
                    |networks, connection| {
                        let is_connected = active_conns.iter().any(|conn| match conn {
                            ActiveConnectionInfo::Wired { name, .. } => {
                                name.as_str() == connection.id.as_str()
                            }

                            _ => false,
                        });

                        let (connect_txt, connect_msg) = if is_connected {
                            (&section.descriptions[connected_txt], None)
                        } else {
                            (
                                &section.descriptions[connect_txt],
                                Some(Message::Activate(connection.uuid.clone())),
                            )
                        };

                        let identifier = widget::text::body(&connection.id).wrap(Wrap::Glyph);

                        let connect = widget::button::text(connect_txt).on_press_maybe(connect_msg);

                        let view_more_button =
                            widget::button::icon(widget::icon::from_name("view-more-symbolic"));

                        let view_more: Option<Element<_>> = if page
                            .view_more_popup
                            .as_deref()
                            .map_or(false, |id| id == connection.uuid.as_ref())
                        {
                            widget::popover(view_more_button.on_press(Message::ViewMore(None)))
                                .position(widget::popover::Position::Bottom)
                                .on_close(Message::ViewMore(None))
                                .popup({
                                    widget::column()
                                        .push_maybe(is_connected.then(|| {
                                            popup_button(
                                                Message::Deactivate(connection.uuid.clone()),
                                                &section.descriptions[disconnect_txt],
                                            )
                                        }))
                                        .push(popup_button(
                                            Message::Settings(connection.uuid.clone()),
                                            &section.descriptions[settings_txt],
                                        ))
                                        // TODO: message not handled yet
                                        // .push_maybe(has_multiple_connection_profiles.then(|| {
                                        //     popup_button(
                                        //         Message::RemoveProfile(connection.uuid.clone()),
                                        //         &section.descriptions[remove_txt],
                                        //     )
                                        // }))
                                        .width(Length::Fixed(170.0))
                                })
                                .apply(|e| Some(Element::from(e)))
                        } else {
                            view_more_button
                                .on_press(Message::ViewMore(Some(connection.uuid.clone())))
                                .apply(|e| Some(Element::from(e)))
                        };

                        let controls = widget::row::with_capacity(2)
                            .push(connect)
                            .push_maybe(view_more)
                            .spacing(spacing.space_xxs);

                        let widget = widget::settings::item_row(vec![
                            identifier.into(),
                            widget::horizontal_space(Length::Fill).into(),
                            controls.into(),
                        ]);

                        networks.add(widget)
                    },
                );

                view = view.push(known_networks);
            }

            view.spacing(spacing.space_l)
                .apply(Element::from)
                .map(crate::pages::Message::Wired)
        })
}

fn popup_button<'a>(message: Message, text: &'a str) -> Element<'a, Message> {
    widget::text::body(text)
        .vertical_alignment(alignment::Vertical::Center)
        .apply(widget::button)
        .padding([4, 16])
        .width(Length::Fill)
        .style(cosmic::theme::Button::MenuItem)
        .on_press(message)
        .into()
}

pub fn update_state(conn: zbus::Connection) -> Command<crate::app::Message> {
    cosmic::command::future(async move {
        match NetworkManagerState::new(&conn).await {
            Ok(state) => Message::UpdateState(state),
            Err(why) => Message::Error(why.to_string()),
        }
    })
    .map(crate::pages::Message::Wired)
    .map(crate::app::Message::PageMessage)
}

pub fn update_devices(conn: zbus::Connection) -> Command<crate::app::Message> {
    cosmic::command::future(async move {
        let filter =
            |device_type| matches!(device_type, network_manager::devices::DeviceType::Ethernet);

        match network_manager::devices::list(&conn, filter).await {
            Ok(devices) => Message::UpdateDevices(devices),
            Err(why) => Message::Error(why.to_string()),
        }
    })
    .map(crate::pages::Message::Wired)
    .map(crate::app::Message::PageMessage)
}

use std::{collections::HashMap, ops::{Deref, DerefMut}};
use eui48::MacAddress;
use pcap::{Capture, Device};
use radiotap::Radiotap;
use oui::{OuiDatabase, OuiEntry};
use clap::{Arg, App};
use termion::{event::Key, input::{MouseTerminal, TermRead}, raw::IntoRawMode, screen::AlternateScreen};
use tui::{
    backend::TermionBackend,
    layout::{Constraint, Direction, Layout},
    widgets::{BarChart, Block, Borders, List, ListItem, Tabs},
    style::{Style, Modifier, Color},
    text::{Span, Spans}
};
use wifi::Frame;

mod ui;
mod wifi;
mod page;

fn main() {
    let args = App::new("Blockade Recon 2")
        .version(env!("CARGO_PKG_VERSION"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .arg(
            Arg::with_name("interface")
            .short("i")
            .long("interface")
            .help("Don't pick a default wireless interface to sniff traffic on")
        )
        .get_matches();
    
    println!("Parsing Manufacturer Names");
    let oui_db = OuiDatabase::new_from_str(include_str!("oui_database")).expect("Failed to parse MAC address lookup database");
    let backend = TermionBackend::new(
        AlternateScreen::from(
            MouseTerminal::from(
                std::io::stdout().into_raw_mode().expect("Unable to switch stdout to raw mode")
            )
        )
    );
    let mut terminal = tui::Terminal::new(backend).expect("Unable to create TUI");
    let input = ui::Input::new();
    
    let mut device = if args.is_present("interface") {
        let devices = Device::list().expect("Unable to find devices");
        let devices_names: Vec<_> = devices.iter().map(|d| ListItem::new(vec![Spans::from(d.name.as_str())])).collect();
        let list = List::new(devices_names)
            .block(Block::default().borders(Borders::ALL).title("Select a WiFi Device"))
            .highlight_style(Style::default().bg(Color::Reset).add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");
        let mut list_state = ui::ListState::with_item_count(devices.len());

        let mut draw = |list: &List, list_state: &mut ui::ListState| terminal.draw(|f| {
            f.render_stateful_widget(list.clone(), f.size(), list_state)
        }).expect("Unable to create list widget");
        draw(&list, &mut list_state);
        'select_device: loop {
            for key in input.stdin.iter() {
                match key {
                    Key::Esc => return,
                    Key::Up | Key::Char('w') => { list_state.up(); draw(&list, &mut list_state) }
                    Key::Down | Key::Char('s') => { list_state.down(); draw(&list, &mut list_state) }
                    Key::PageUp => { list_state.top(); draw(&list, &mut list_state) }
                    Key::PageDown => { list_state.bottom(); draw(&list, &mut list_state) }
                    Key::Char('\n') => break 'select_device devices[list_state.selected().unwrap()].clone(),
                    _ => ()
                }
            }
        }
    } else {
        Device::lookup().expect("Unable to choose a default device")
    };

    let mut capture = Capture::from_device(device).unwrap()
        .promisc(true)
        .rfmon(true)
        .immediate_mode(true)
        .open().unwrap();
    let mut savefile = capture.savefile("capture.pcap").unwrap();

    if capture.get_datalink() != pcap::Linktype::IEEE802_11_RADIOTAP {
        let mut ok = false;
        for datalink in capture.list_datalinks().expect("Unable to determine supported datalink layers") {
            if datalink == pcap::Linktype::IEEE802_11_RADIOTAP {
                ok = true;
                capture.set_datalink(datalink).expect("Unable to set the datalink layer")
            }
        }
        if !ok {
            panic!("The interface does not support the radiotap datalink layer required by this program")
        }
    }

    let mut devices = DeviceList::default();
    let pages: &mut [&mut dyn page::Page] = &mut [&mut page::Devices::new(), &mut page::Manufacturers::new()];
    let mut tabs = ui::TabState::new(pages.iter().map(|p| Spans::from(p.name())).collect());
    'sniff: loop {
        for key in input.stdin.try_iter() {
            match key {
                Key::Esc => break 'sniff,
                Key::F(i) => tabs.select(i as usize),
                Key::Char('\t') => tabs.next(),
                Key::Up | Key::Char('w') => pages[tabs.index].up(),
                Key::Down | Key::Char('s') => pages[tabs.index].down(),
                Key::PageUp => pages[tabs.index].top(),
                Key::PageDown => pages[tabs.index].bottom(),
                _ => ()
            }
        }

        terminal.draw(|frame| {
            let areas = Layout::default()
                .direction(Direction::Vertical)
                .margin(0)
                .constraints([Constraint::Length(2), Constraint::Min(0)])
                .split(frame.size());
            frame.render_widget(
                Tabs::new(tabs.titles.clone())
                    .block(Block::default().borders(Borders::BOTTOM))
                    .select(tabs.index)
                    .style(Style::reset())
                    .highlight_style(Style::reset().add_modifier(Modifier::BOLD | Modifier::REVERSED)),
                areas[0]
            );
            pages[tabs.index].render(frame, areas[1], &mut devices)
        }).expect("Unable to draw to stdout");

        let packet = capture.next().unwrap();
        savefile.write(&packet);
        
        let (radiotap, data) = Radiotap::parse(packet.data).unwrap();
        use wifi::Frame::*;
        if let Ok(frame) = wifi::Frame::parse(data) {
            match frame {
                Beacon {
                    source,
                    destination,
                    ssid,
                    ..
                } => {
                    devices.get_or_default(source, &oui_db)
                        .sent()
                        .beacon(ssid);
                    devices.get_or_default(destination, &oui_db);
                }
                Ack {
                    receiver
                } => {
                    devices.get_or_default(receiver, &oui_db);
                }
                _ => ()
            }
        }
    }
    std::mem::drop(terminal);
    println!("Found:\n{:?}", devices);
}

/// A device tracked by blockade
/// Tracks metadata relating to the device
#[derive(Debug)]
pub struct KnownDevice {
    manufacturer: Option<OuiEntry>,
    /// The SSID of the beacon, or None if not a beacon
    beacon: Option<String>,
    /// False if this device is known only by reference from another device, ie. has not sent any data
    sent: bool,
}
impl KnownDevice {
    fn new(address: MacAddress, oui_db: &OuiDatabase) -> Self {
        Self {
            manufacturer: oui_db.query_by_mac(&address).unwrap(/* Library should never be able to return an error */),
            beacon: None,
            sent: false
        }
    }
    fn sent(&mut self) -> &mut Self {
        self.sent = true;
        self
    }
    fn beacon(&mut self, ssid: String) -> &mut Self {
        self.beacon = Some(ssid);
        self
    }
}

#[derive(Debug, Default)]
pub struct DeviceList(HashMap<MacAddress, KnownDevice>);
impl DeviceList {
    fn get_or_default(&mut self, address: MacAddress, oui_db: &OuiDatabase) -> &mut KnownDevice {
        if self.contains_key(&address) {
            self.get_mut(&address).unwrap()
        } else {
            self.insert(address, KnownDevice::new(address, oui_db));
            self.get_mut(&address).unwrap()
        }
    }
    pub fn bar_data(&self) -> Vec<(&str, u64)> {
        let mut manufacturers = HashMap::new();
        for device in self.values() {
            if let Some(OuiEntry { name_short, ..}) = &device.manufacturer {
                if let Some(count) = manufacturers.get_mut(name_short.as_str()) {
                    *count += 1
                } else {
                    manufacturers.insert(name_short.as_str(), 1u64);
                }
            }
        }
        let mut values: Vec<(&str, u64)> = manufacturers.iter().map(|(&name, &count)| (name, count)).collect();
        use std::cmp::Reverse;
        values.sort_by(|(nl, l), (nr, r)| l.cmp(r).then_with(|| nl.cmp(nr)));
        values.reverse();
        values
        //&[("Apples and Oranges", 3)]
    }
}
impl Deref for DeviceList {
    type Target = HashMap<MacAddress, KnownDevice>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for DeviceList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
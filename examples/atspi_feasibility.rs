//! Feasibility probe: can the project's existing `zbus::blocking` style drive
//! AT-SPI unaided, and does the element tree come back with usable geometry?
//!
//! Throwaway: `cargo run --example atspi_feasibility`.  Not part of the build.

use zbus::blocking::Connection;
use zbus::blocking::Proxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

/// One AT-SPI node, as the protocol names it: a bus name plus a path.
#[derive(Debug, Clone)]
struct NodeRef {
    bus: String,
    path: String,
}

fn main() {
    // Step 1: the a11y bus is a *separate* bus, reached through the session
    // bus's `org.a11y.Bus.GetAddress`.
    let session = Connection::session().expect("session bus");
    let address: String = session
        .call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.a11y.Bus"),
            "GetAddress",
            &(),
        )
        .expect("GetAddress")
        .body()
        .deserialize()
        .expect("deserialize address");
    println!("a11y bus address: {address}");

    let a11y = zbus::blocking::connection::Builder::address(address.as_str())
        .expect("bad address")
        .build()
        .expect("connect a11y bus");
    println!("connected to a11y bus");

    // Step 2: walk root -> desktop(es) -> applications -> frames.
    let root = NodeRef {
        bus: "org.a11y.atspi.Registry".into(),
        path: "/org/a11y/atspi/accessible/root".into(),
    };

    let count = child_count(&a11y, &root);
    println!("root childCount = {count:?}");

    for i in 0..count.unwrap_or(0) {
        let Some(child) = child_at(&a11y, &root, i) else {
            continue;
        };
        let name = name(&a11y, &child).unwrap_or_default();
        println!("  [{i}] {name}  ({:?})", child);
    }

    // Step 3: for each application, print its frames and a shallow tree.
    for i in 0..count.unwrap_or(0) {
        let Some(app) = child_at(&a11y, &root, i) else {
            continue;
        };
        let app_name = name(&a11y, &app).unwrap_or_default();
        let n = child_count(&a11y, &app).unwrap_or(0);
        if n == 0 {
            continue;
        }
        println!("\napp {app_name:?} ({n} frames)");
        for j in 0..n {
            let Some(frame) = child_at(&a11y, &app, j) else {
                continue;
            };
            let frame_name = name(&a11y, &frame).unwrap_or_default();
            println!("  frame {j}: {frame_name}");
            walk(&a11y, &frame, 0, 4);
        }
    }
}

fn walk(conn: &Connection, node: &NodeRef, depth: usize, max: usize) {
    if depth > max {
        return;
    }
    let name = name(conn, node).unwrap_or_default();
    let role = role_name(conn, node).unwrap_or_default();
    let window = extents(conn, node, 1);
    let screen = extents(conn, node, 0);
    println!(
        "{}  {}{}  win={:?} scr={:?}",
        "    ".repeat(depth + 2),
        role,
        if name.is_empty() {
            String::new()
        } else {
            format!(" {name:?}")
        },
        window,
        screen
    );
    let n = child_count(conn, node).unwrap_or(0);
    for i in 0..n {
        if let Some(child) = child_at(conn, node, i) {
            walk(conn, &child, depth + 1, max);
        }
    }
}

fn proxy<'a>(conn: &'a Connection, bus: &'a str, path: &'a str) -> Proxy<'a> {
    let object_path = ObjectPath::try_from(path).expect("path");
    Proxy::new(conn, bus, object_path, "org.a11y.atspi.Accessible").expect("proxy")
}

fn props<'a>(conn: &'a Connection, bus: &'a str, path: &'a str) -> Proxy<'a> {
    let object_path = ObjectPath::try_from(path).expect("path");
    Proxy::new(conn, bus, object_path, "org.freedesktop.DBus.Properties").expect("proxy")
}

/// Reads one property through an explicit `Properties.Get`: zbus's own
/// `get_property` prefers `GetAll`, which the AT-SPI root does not answer.
fn prop<T>(conn: &Connection, node: &NodeRef, interface: &str, name: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
    T::Error: Into<zbus::Error>,
{
    let p = props(conn, &node.bus, &node.path);
    let reply = p.call_method("Get", &(interface, name)).ok()?;
    let (value,): (OwnedValue,) = reply.body().deserialize().ok()?;
    T::try_from(value).ok()
}

fn child_count(conn: &Connection, node: &NodeRef) -> Option<i32> {
    prop::<i32>(conn, node, "org.a11y.atspi.Accessible", "ChildCount")
}

fn name(conn: &Connection, node: &NodeRef) -> Option<String> {
    prop::<String>(conn, node, "org.a11y.atspi.Accessible", "Name")
}

fn child_at(conn: &Connection, node: &NodeRef, index: i32) -> Option<NodeRef> {
    let p = proxy(conn, &node.bus, &node.path);
    let reply = p.call_method("GetChildAtIndex", &(index)).ok()?;
    let (bus, path): (String, OwnedObjectPath) = reply.body().deserialize().ok()?;
    Some(NodeRef {
        bus,
        path: path.to_string(),
    })
}

fn role_name(conn: &Connection, node: &NodeRef) -> Option<String> {
    let p = proxy(conn, &node.bus, &node.path);
    let reply = p.call_method("GetRoleName", &()).ok()?;
    let s: String = reply.body().deserialize().ok()?;
    Some(s)
}

/// `coord_type`: 0 = screen, 1 = window.
fn extents(conn: &Connection, node: &NodeRef, coord_type: u32) -> Option<(i32, i32, i32, i32)> {
    let object_path = ObjectPath::try_from(node.path.as_str()).ok()?;
    let p = Proxy::new(
        conn,
        node.bus.as_str(),
        object_path,
        "org.a11y.atspi.Component",
    )
    .ok()?;
    let reply = p.call_method("GetExtents", &(coord_type)).ok()?;
    let (x, y, w, h): (i32, i32, i32, i32) = reply.body().deserialize().ok()?;
    Some((x, y, w, h))
}

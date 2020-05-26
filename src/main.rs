//! This is a simple demo app to start testing the automerge-rs library. In
//! order to understand the following you should be familiar with automerge
//! and in particular with the reason for the backend/frontend split. 
//!
//! In this example I'm interested in seeing how hard the automerge-rs API is
//! to use for GUI applications doing real time text editing. I am not
//! interested in understanding how network and storage will be integrated, as
//! such this application starts two windows, each one of which has its own
//! instance of the frontend, and communicates via crossbeam channels with
//! its own instance of the backend. Each window immediately applies changes on
//! its frontend, then sends the resulting change request to a 
//! crossbeam::Sender<automerge_protocol::Sender> channel. A separate thread
//! pulls change requests out of the other end of those channels, applies them
//! to each of the backends, then sends the corresponding patches back to the
//! frontend via a vgtk scope.

#![recursion_limit = "512"]
use vgtk::ext::*;
use vgtk::lib::gio::{ApplicationFlags, prelude::ApplicationExtManual};
use vgtk::lib::gtk::*;
use vgtk::lib::gtk::prelude::TextBufferExtManual;
use vgtk::lib::glib::{SignalHandlerId, ObjectExt};
use vgtk::{gtk, start, Component, UpdateAction, VNode, Callback};
use automerge_frontend::{Frontend, LocalChange, Path, Value};
use automerge_backend::Backend;
use automerge_protocol as amp;
use std::cell::RefCell;
use std::rc::Rc;

/// A wrapper around the state of the frontend, this is passed to DocView as a
/// property. 
struct Doc {
    frontend: Rc<RefCell<Frontend>>,
    buffer: TextBuffer,
    /// We need these two signal handlers to block the signals when updating
    /// the text based on diffs received from the backend
    insert_text_sigid: SignalHandlerId,
    del_sig_id: SignalHandlerId,
    /// This is the channel we use to send new changes to the backend
    sx: crossbeam::Sender<amp::Request>,
}


impl Doc {
    fn new(sx: crossbeam::Sender<amp::Request>) -> Doc {
        let mut frontend = Frontend::new();
        // Initialize the state of the frontend to
        // {
        //     "counts": Counter(0),
        //     "text": ""
        // }
        let cr = frontend.change(None, |doc| {
            doc.add_change(LocalChange::set(
                Path::root().key("counts"),
                Value::Primitive(amp::Value::Counter(0)),
            ))?;
            doc.add_change(LocalChange::set(
                Path::root().key("text"),
                Value::Sequence(Vec::new(), amp::SequenceType::Text),
            ))?;
            Ok(())
        }).unwrap().unwrap();
        let sx_clone = sx.clone();
        let sx_clone_2 = sx.clone();
        // Send the initialization change request to the backend
        sx.try_send(cr).unwrap();


        let frontend_rf = Rc::new(RefCell::new(frontend));
        let buffer = TextBuffer::new::<TextTagTable>(None);
        let frontend_clone = frontend_rf.clone();

        // Wire up the insert text signal handler
        let sig_id = buffer.connect_insert_text(move |_, iter, i| {
            let pos = iter.get_offset();
            // Add the change to the frontend
            let cr = frontend_clone.borrow_mut().change(None, |doc| {
                doc.add_change(LocalChange::insert(
                    Path::root().key("text").index(pos as usize),
                    Value::Primitive(i.into())
                ))?;
                Ok(())
            }).unwrap();

            // Send the change request to the backend
            if let Some(r) = cr {
                sx_clone.send(r).unwrap();
            }
        });

        let second_frontend_clone = frontend_rf.clone();

        // Wire up the delete text handler
        let del_sig_id = buffer.connect_delete_range(move |_, start, end| {
            // For each deleted character, add the change to the frontend
            // and send the change request to the backend
            for i in start.get_offset()..end.get_offset() {
                let cr = second_frontend_clone.borrow_mut().change(None, |doc| {
                    doc.add_change(LocalChange::delete(
                        Path::root().key("text").index((i) as usize)
                    ))?;
                    Ok(())
                }).unwrap();
                if let Some(r) = cr {
                    sx_clone_2.send(r).unwrap();
                }
            };
        });

        Doc{
            frontend: frontend_rf,
            buffer,
            insert_text_sigid: sig_id,
            del_sig_id,
            sx,
        }
    }

    /// Apply the patch and update the text buffer if necessary
    fn apply_patch(&mut self, patch: Option<amp::Patch>) {
        if let Some(patch) = patch {
            self.frontend.borrow_mut().apply_patch(patch.clone()).unwrap();
            // We don't need to update the text buffer if it's a patch from ourselves
            if patch.actor == Some(self.frontend.borrow().actor_id.to_string()) {
                return
            };
            // We have to block these signals otherwise the handlers will fire
            // as we update the text, which will cause a loop
            self.buffer.block_signal(&self.insert_text_sigid);
            self.buffer.block_signal(&self.del_sig_id);
            let text = match self.frontend.borrow().get_value(&Path::root().key("text")) {
                Some(Value::Sequence(vals, amp::SequenceType::Text)) => {
                    vals.iter().map(|v| match v {
                        Value::Primitive(amp::Value::Str(s)) => s.to_string(),
                        _ => "".to_string(),
                    })
                    .collect::<Vec<String>>()
                    .join("")
                },
                _ => "".to_string()
            };
            self.buffer.set_text(text.as_str());
            self.buffer.unblock_signal(&self.insert_text_sigid);
            self.buffer.unblock_signal(&self.del_sig_id);
        }
    }

    /// Get the value of the counter
    fn counter_value(&self) -> i64 {
        match self.frontend.borrow_mut().state() {
            Value::Map(vals, _) => vals.get("counts").and_then(|c| match c {
                Value::Primitive(amp::Value::Counter(i)) => Some(*i),
                _ => None
            }),
            _ => None,
        }.unwrap_or(0)
    }

    /// Increment the counter value locally and send the corresponding 
    /// change to the backend
    fn inc_counter(&mut self) -> () {
        let cr = self.frontend.borrow_mut().change(None, |doc| {
            doc.add_change(LocalChange::increment(
                Path::root().key("counts")
            ))?;
            Ok(())
        }).unwrap();
        if let Some(cr) = cr {
            self.sx.send(cr).unwrap();
        }
    }
}

#[derive(Default)]
struct DocView {
    doc: Option<Rc<RefCell<Doc>>>,
    on_exit: Callback<()>,
}

#[derive(Debug, Clone)]
enum DocMessage {
    Inc,
    Exit,
}

#[derive(Clone, Default)]
struct DocViewProperties {
    doc: Option<Rc<RefCell<Doc>>>,
    on_exit: Callback<()>
}

impl Component for DocView {
    type Message = DocMessage;
    type Properties = DocViewProperties;
    fn view(&self) -> VNode<Self> {
        match &self.doc {
            // We're waiting for the outer component to give us a doc
            None => gtk!{
                <Window title="Doc 1" border_width=20 default_width=1000 default_height=500 on destroy=|_| DocMessage::Exit>
                    <HeaderBar title="inc" show_close_button=true />
                    <Box orientation=Orientation::Vertical valign=Align::Center halign=Align::Center vexpand=true>
                        <Label label="Initializing" />
                    </Box>
                </Window>
            },
            Some(doc) => gtk!{
                <Window title="Doc 1" border_width=20 default_width=1000 default_height=500 on destroy=|_| DocMessage::Exit>
                    <HeaderBar title="inc" show_close_button=true />
                    <Box orientation=Orientation::Vertical valign=Align::Center halign=Align::Center vexpand=true>
                        <Label label="Counter" />
                        <Box spacing=30 halign=Align::Center valign=Align::Center orientation=Orientation::Horizontal Box::expand=false>
                            <Label label=doc.borrow().counter_value().to_string() />
                            <Button label="inc!" image="list-add" Box::expand=false always_show_image=true on clicked=|_| DocMessage::Inc />
                        </Box>
                        <Label label="Text" />
                        <TextView buffer=Some(doc.borrow().buffer.clone()) />
                    </Box>
                </Window>
            }
        }
    }

    fn change(&mut self, properties: Self::Properties) -> UpdateAction<Self> {
        self.doc = properties.doc;
        UpdateAction::Render
    }

    fn update(&mut self, msg: Self::Message) -> UpdateAction<Self> {
        match msg {
            DocMessage::Inc => {
                self.doc.as_mut().map(|d| d.borrow_mut().inc_counter());
                UpdateAction::Render
            },
            DocMessage::Exit => {
                self.on_exit.send(());
                UpdateAction::None
            }
        }
    }
}

#[derive(Default)]
struct Model {
    doc1: Option<Rc<RefCell<Doc>>>,
    doc2: Option<Rc<RefCell<Doc>>>,
}


#[derive(Clone, Debug)]
enum Message {
    Exit,
    /// Fired once the backend thread has started and we have senders to give
    /// to our docs
    Initialized{
        sx1: crossbeam::Sender<amp::Request>,
        sx2: crossbeam::Sender<amp::Request>,
    },
    /// Pushed into the application scope by the backend thread when new
    /// patches are received
    Patch {
        doc1: Option<amp::Patch>,
        doc2: Option<amp::Patch>,
    }
}

impl Component for Model {
    type Message = Message;
    type Properties = ();

    fn update(&mut self, msg: Self::Message) -> UpdateAction<Self> {
        match msg {
            Message::Exit => {
                vgtk::quit();
                UpdateAction::None
            }
            Message::Initialized{sx1, sx2} => {
                self.doc1 = Some(Rc::new(RefCell::new(Doc::new(sx1))));
                self.doc2 = Some(Rc::new(RefCell::new(Doc::new(sx2))));
                UpdateAction::Render
            },
            Message::Patch{doc1: patch1, doc2: patch2} => {
                self.doc1.as_mut().map(|d| d.borrow_mut().apply_patch(patch1));
                self.doc2.as_mut().map(|d| d.borrow_mut().apply_patch(patch2));
                UpdateAction::Render
            },
        }
    }

    fn view(&self) -> VNode<Model> {
        gtk! {
            <Application::new_unwrap(Some("com.example.automerge-demo"), ApplicationFlags::empty())>
                <@DocView doc=self.doc1.clone() on exit=|_| Message::Exit />
                <@DocView doc=self.doc2.clone() on exit=|_| Message::Exit />
            </Application>
        }
    }
}

fn main() {
    pretty_env_logger::init();
    let (app, scope) = start::<Model>();
    let args: Vec<String> = std::env::args().collect();
    let (sx1, rx1) = crossbeam::channel::unbounded();
    let (sx2, rx2) = crossbeam::channel::unbounded();
    let (closesx, closerx) = crossbeam::channel::unbounded::<()>();
    let scope_clone = scope.clone();

    let backend_thread = std::thread::spawn(move || {
        let mut backend1 = Backend::init();
        let mut backend2 = Backend::init();
        loop {
            crossbeam::select!{
                recv(rx1) -> msg => {
                    let patch1 = backend1.apply_local_change(msg.unwrap()).unwrap();
                    let patch2 = backend2.apply_changes(backend1.get_changes(&[]).iter().copied().cloned().collect()).unwrap();
                    scope.try_send(Message::Patch{doc1: Some(patch1), doc2: Some(patch2)}).unwrap();
                }
                recv(rx2) -> msg => {
                    let patch2 = backend2.apply_local_change(msg.unwrap()).unwrap();
                    let patch1 = backend1.apply_changes(backend2.get_changes(&[]).iter().copied().cloned().collect()).unwrap();
                    scope.try_send(Message::Patch{doc1: Some(patch1), doc2: Some(patch2)}).unwrap();
                }
                recv(closerx) -> _ => return
            }
        };
    });
    scope_clone.send_message(Message::Initialized{sx1, sx2});

    app.run(&args);
    closesx.send(()).unwrap();
    backend_thread.join().unwrap();
}

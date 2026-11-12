#!/usr/bin/env python3
"""Actual native drag, cancel, keyboard, edge scroll and persisted-ID proof."""
import json
import os
import signal
import subprocess
import time
from accessibility import items, snapshot
from local_murmur import eventually
from layout_regression import inside
from mumble_client import Client


def run(args, root, shell, registry, bots, profile, port, pin, shot, app):
    def application():return next((node for node in registry.children() if node.name.lower()=='nuschl'),None)
    def named(role,name,prefix=False):
        current=application()
        return next((node for node,r,text,_ in items(current) if (not role or role==r) and (text.startswith(name) if prefix else text==name)),None) if current else None
    def bookmarks():return json.loads((profile/'config/servers.json').read_text())
    original=bookmarks();names={server['id']:server['name'] for server in original}
    def saved():return [server['id'] for server in bookmarks()]
    def tile(id):return named('push button',names[id]+';',True)
    def point(node):
        g=json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return {x:b.x,y:b.y};})())"));x,y=node.centre();return g['x']+x,g['y']+y
    def visible(node):
        node.call('Component','GrabFocus');eventually(lambda:inside(node.extents(),named('list','Servers').extents()),'server not revealed')
    def button(down):shell.eval(f"global._nuschlPointer.notify_button(imports.gi.GLib.get_monotonic_time(),1,imports.gi.Clutter.ButtonState.{'PRESSED' if down else 'RELEASED'}); 'ok'")
    def begin(id):
        node=tile(id);visible(node);shell.move(*point(node));time.sleep(.2);button(True);x,y=point(node);shell.move(x+5,y+3);time.sleep(.1)
    def ui_order():
        by_name={name:id for id,name in names.items()}
        return [by_name[name.split(';')[0]] for node,role,name,_ in items(named('list','Servers')) if role=='push button' and name.split(';')[0] in by_name]
    records=[]
    report=args.output/'order-proof.json'
    first,last=original[0]['id'],original[-1]['id']
    eventually(lambda:named('push button','Unmute'),'Unmute absent').press()
    eventually(lambda:'talking here' in tile(first).name,'primary voice not live')
    visible(tile(last));tile(last).press()
    eventually(lambda:named('push button','Connect'),'last Connect absent').press()
    eventually(lambda:'Connected' in tile(last).name,'last server not connected')
    second=original[-1]
    witness=Client(args.binaries,'127.0.0.1',second['port'],second['certificate_sha256'],'Order witness',root/'order-witness');bots.append(witness)
    session=eventually(lambda:next((user['session'] for user in bots[0].users.values() if user['name']=='Andi'),None),'Andi absent')
    peer=eventually(lambda:next((user['session'] for user in witness.users.values() if user['name']=='Rail Last'),None),'last user absent')
    composer=eventually(lambda:named('entry','Message'),'composer absent')
    composer.call('Component','GrabFocus')
    eventually(lambda:composer.focused,'composer was not focused')
    shell.type('Draft survives moving servers')
    def invariant():
        assert 'talking here' in tile(first).name,'voice moved while reordering'
        assert named(None,'Rail last') is not None,'text selection changed'
        assert any(user['name']=='Andi' and user['session']==session and user['channel']==2 for user in bots[0].users.values()),'primary connection/channel changed'
        assert any(user['name']=='Rail Last' and user['session']==peer and user['channel']==0 for user in witness.users.values()),'secondary connection changed'
    time.sleep(1.2)
    listing=named('list','Servers');listing.call('Component','GrabFocus');shell.key('Home');time.sleep(.2)
    baseline=saved();stamp=(profile/'config/servers.json').stat().st_mtime_ns
    begin(first);target=tile(original[3]['id']);shell.move(*point(target));time.sleep(.15)
    preview=ui_order();shot('order-preview')
    assert preview!=baseline,'drag did not preview a changed order'
    assert saved()==baseline,'preview persisted before drop'
    assert (profile/'config/servers.json').stat().st_mtime_ns==stamp,'drag wrote bookmark file'
    button(False);eventually(lambda:saved()==preview,'drop was not persisted')
    invariant();records.append({'drop':preview,'voice_session':session,'peer_session':peer});report.write_text(json.dumps(records,indent=2))
    # Escape and dropping outside the rail both restore preview without writes.
    for cancel in ['Escape','outside']:
        before=saved();begin(first);target=tile(before[0]);shell.move(*point(target));time.sleep(.1)
        assert ui_order()!=before,f'{cancel} did not start a changed preview'
        if cancel=='Escape':shell.key('Escape')
        else:
            window=shell.window('Nuschl');shell.move(window['x']+window['width']-180,window['y']+180)
        button(False);eventually(lambda:ui_order()==before,f'{cancel} did not restore canonical order')
        assert saved()==before,f'{cancel} persisted a preview';invariant();records.append({'cancel':cancel,'order':before})
    # Real keyboard chord and context actions remain accessible alternatives.
    visible(tile(first));before=saved();position=before.index(first)
    shell.eval("const k=global._nuschlKeyboard,t=imports.gi.GLib.get_monotonic_time(),C=imports.gi.Clutter; k.notify_keyval(t,C.KEY_Control_L,C.KeyState.PRESSED); k.notify_keyval(t+1000,C.KEY_Shift_L,C.KeyState.PRESSED); k.notify_keyval(t+2000,C.KEY_Up,C.KeyState.PRESSED); k.notify_keyval(t+3000,C.KEY_Up,C.KeyState.RELEASED); k.notify_keyval(t+4000,C.KEY_Shift_L,C.KeyState.RELEASED); k.notify_keyval(t+5000,C.KEY_Control_L,C.KeyState.RELEASED); 'ok'")
    expected=before.copy();expected.insert(position-1,expected.pop(position));eventually(lambda:saved()==expected,'keyboard reorder failed')
    visible(tile(first));shell.click(*point(tile(first)),button=3)
    eventually(lambda:named('menu item','Move server later'),'context alternative absent').press()
    eventually(lambda:saved()==before,'context reorder failed');invariant();records.append({'keyboard_and_context':True})
    # A stationary edge hold must scroll a long rail without persisting frames.
    begin(first);g=json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return {x:b.x,y:b.y};})())"))
    x,y,w,h=listing.extents();shell.move(g['x']+x+w/2,g['y']+y+h-8)
    old_y=tile(before[0]).extents()[1];time.sleep(1.1)
    assert tile(before[0]).extents()[1]<old_y-80,'stationary edge drag did not autoscroll'
    assert saved()==before,'edge preview persisted early'
    # Stop the edge hold before sampling the final target: screenshot/AT-SPI
    # round trips must not race a still-moving autoscroll insertion point.
    shell.move(g['x']+x+w/2,g['y']+y+h/2);time.sleep(.3)
    expected=ui_order();assert expected!=before
    shot('order-edge-scroll');button(False);eventually(lambda:saved()!=before,'edge drop did not persist')
    assert saved()==expected,{'expected':expected,'actual':saved()}
    invariant()
    records.append({'edge_drop':expected});report.write_text(json.dumps(records,indent=2))
    # GPUI-kit inputs currently do not expose AT-SPI Text values. Independently
    # prove the original draft survived every action by sending it ONCE now.
    eventually(lambda:named('push button','Send'),'Send absent').press()
    delivered=witness.wait(lambda event:event.get('kind')=='text' and event.get('body')=='Draft survives moving servers',timeout=15)
    assert delivered.get('author')=='Rail Last',delivered
    records.append({'draft_delivered_after_all_actions':True})
    report.write_text(json.dumps(records,indent=2))
    # A genuine process reload reads the persisted order, no live-vector trick.
    os.killpg(app.pid,signal.SIGTERM);app.wait(timeout=10)
    eventually(lambda:application() is None,'old accessibility application remained')
    log=(args.output/'restart.log').open('w')
    restarted=subprocess.Popen([str(args.binaries/'nuschl'),'--profile-dir',str(profile)],env=shell.app_environment(),stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
    try:
        eventually(lambda:application() is not None and ui_order()==expected,'restart did not restore saved rail order')
        shot('order-reloaded');records.append({'reload':True,'order':expected});report.write_text(json.dumps(records,indent=2))
    finally:
        if restarted.poll() is None:os.killpg(restarted.pid,signal.SIGTERM);restarted.wait(timeout=10)
        log.close()
    print('Server order: real drop/cancel/keyboard/context/edge/reload and voice/draft invariants passed.',flush=True)

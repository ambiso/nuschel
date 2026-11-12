#!/usr/bin/env python3
"""Native rail geometry and same-frame icon/ring contour regression proof."""
import json
import math
import statistics
import subprocess
import time
from accessibility import items
from local_murmur import eventually
from layout_regression import inside


def run(args, shell, registry, profile, shot):
    app=eventually(lambda:next((n for n in registry.children() if n.name.lower()=="nuschl"),None),"AT-SPI desktop absent")
    def named(role,name,prefix=False):
        return next((n for n,r,text,_ in items(app) if (not role or role==r) and (text.startswith(name) if prefix else text==name)),None)
    def geometry():
        return json.loads(shell.eval("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const f=w.get_frame_rect(),b=w.get_buffer_rect();return {frame:{x:f.x,y:f.y,width:f.width,height:f.height},buffer:{x:b.x,y:b.y,width:b.width,height:b.height}};})())"))
    def point(node):
        g=geometry();x,y=node.centre();return g['buffer']['x']+x,g['buffer']['y']+y
    def capture(name):
        path=args.output/f'{name}.png';shell.screenshot(path)
        return path
    def pixels(path):
        size=subprocess.check_output(['magick',str(path),'-format','%w %h','info:'],text=True).split()
        w,h=map(int,size)
        raw=subprocess.check_output(['magick',str(path),'-depth','8','rgb:-'])
        assert len(raw)==w*h*3
        def at(x,y):
            i=(max(0,min(h-1,round(y)))*w+max(0,min(w-1,round(x))))*3
            return tuple(raw[i:i+3])
        return at
    def contour(path,node,scale):
        at=pixels(path);g=geometry();x,y,w,h=node.extents();x+=g['buffer']['x'];y+=g['buffer']['y']
        tile=at(x+w/2,y+5*scale)
        def near(rgb,other):return max(abs(a-b) for a,b in zip(rgb,other))<=5
        def fit(ox,oy,size,color):
            radii=[]
            for depth in range(max(1,round(scale)),round(12*scale)):
                start=next((dx for dx in range(round(size/2)) if near(at(ox+dx,oy+depth),color)),None)
                if start and start>scale:
                    radii.append(start+depth+math.sqrt(2*start*depth))
            assert radii,f'no contour pixels in {path}'
            return statistics.median(radii)/scale
        inner=fit(x,y,w,tile)
        outer=fit(x-4*scale,y-4*scale,w+8*scale,(60,207,142))
        assert abs(outer-inner-4)<3.2, f'icon/outline shape mismatch {path}: {inner}, {outer}'
        return {'tile_radius':inner,'ring_radius':outer}
    first=eventually(lambda:named('push button','Screenshot server;',True),'first rail absent')
    eventually(lambda:named('push button','Unmute'),'Unmute absent').press()
    eventually(lambda:named('push button','Screenshot server;',True) if 'Microphone destination · enabled' in first.name else None,'first server voice intent not acknowledged')
    # Capture README content from actual private fixture before introducing a
    # long list. The scene's extra servers remain local/offline bookmarks.
    records=[]
    for endpoint,name,dummy in [('first','Screenshot server;','Rail filler 00;'),('last','Rail last;','Rail filler 17;')]:
        node=eventually(lambda:named('push button',name,True),'endpoint absent')
        if endpoint=='last':
            node.call('Component','GrabFocus')
            eventually(lambda:inside(node.extents(),named('list','Servers').extents()),'last endpoint was not revealed before activation')
            node.press()
            eventually(lambda:named(None,'Rail last') is not None,'last endpoint was not selected')
            eventually(lambda:named('push button','Connect'),'last server Connect absent').press()
            try:
                eventually(lambda:'connected' in node.name.lower() and 'disconnected' not in node.name.lower(),'last connection not acknowledged')
            except AssertionError:
                shot('last-connect-failure')
                (args.output/'last-connect-tree.json').write_text(json.dumps([(r,n) for _,r,n,_ in items(app)]))
                raise
            # Route voice through normal context action, without touching any
            # host audio source (the fixture sets a nonexistent private socket).
            shell.click(*point(node),button=3)
            use=eventually(lambda:named('menu item','Use voice here'),'last voice route action absent');use.press()
            # Use voice here transfers the already-live route; wait for its
            # acknowledgment instead of toggling a transient old Unmute label.
            eventually(lambda:'Microphone destination · enabled' in node.name,'last voice intent not acknowledged')
        for percent,steps in [(100,0),(125,2),(150,3),(200,5)]:
            shell.chord('Control_L','0')
            for _ in range(steps):shell.chord('Control_L','equal');time.sleep(.1)
            scale=percent/100
            eventually(lambda:abs(json.loads((profile/'config/settings.json').read_text())['appearance']['zoom']-scale)<.001,'rail zoom missing')
            listing=eventually(lambda:named('list','Servers'),'server list absent')
            # Test real endpoints at their scroll limits, then focus reveal
            # after deliberately scrolling to the opposite end.
            listing.call('Component','GrabFocus');shell.key('Home' if endpoint=='first' else 'End')
            time.sleep(.2)
            bounds=node.extents();paint=(bounds[0]-8*scale,bounds[1]-8*scale,bounds[2]+16*scale,bounds[3]+16*scale)
            assert inside(paint,listing.extents()),f'endpoint decoration gutter clipped {endpoint}/{percent}: {paint} {listing.extents()}'
            listing.call('Component','GrabFocus');shell.key('End' if endpoint=='first' else 'Home')
            eventually(lambda:not inside(node.extents(),listing.extents()),'long list did not scroll')
            node.call('Component','GrabFocus')
            eventually(lambda:inside((node.extents()[0]-8*scale,node.extents()[1]-8*scale,node.extents()[2]+16*scale,node.extents()[3]+16*scale),listing.extents()),'focus omitted decoration gutter')
            node.press()
            other=eventually(lambda:named('push button',dummy,True),'neighbor bookmark absent');other.press()
            # Browsing another server must preserve voice on the endpoint;
            # unselected icons can now exercise circular -> rounded hover.
            if 'Microphone destination · enabled' not in node.name:
                shot(f'route-failure-{endpoint}-{percent}')
                (args.output/'route-failure-tree.json').write_text(json.dumps([(r,n) for _,r,n,_ in items(app)]))
                raise AssertionError(f'{endpoint}/{percent} voice changed after browse: {node.name}')
            node.call('Component','GrabFocus');shell.move(*point(other));time.sleep(.25)
            normal=capture(f'rail-{endpoint}-{percent}-normal');normal_shape=contour(normal,node,scale)
            shell.move(*point(node))
            moving=[]
            for frame in range(5):
                path=capture(f'rail-{endpoint}-{percent}-hover-{frame}')
                moving.append((path,))
            time.sleep(.25)
            hover=capture(f'rail-{endpoint}-{percent}-hover-final');hover_shape=contour(hover,node,scale)
            measured=[contour(path,node,scale) for (path,) in moving]
            assert normal_shape['tile_radius']>21 and hover_shape['tile_radius']<19,f'hover radius did not transition: {normal_shape} {hover_shape}'
            shell.move(*point(other));time.sleep(.25)
            leave=capture(f'rail-{endpoint}-{percent}-leave');leave_shape=contour(leave,node,scale)
            assert leave_shape['tile_radius']>21,'hover leave did not restore circle'
            records.append({'endpoint':endpoint,'zoom':percent,'tile':node.extents(),'viewport':listing.extents(),'normal':normal_shape,'hover':hover_shape,'intermediate':measured,'leave':leave_shape})
            (args.output/'rail-proof.json').write_text(json.dumps(records,indent=2))
    assert any(17.5<frame['tile_radius']<22.5 for row in records for frame in row['intermediate']),'no actual intermediate animation frame captured'
    print('Rail: first/last endpoints, long-list focus/scroll,8 zoom cases and same-frame animated contours passed.',flush=True)

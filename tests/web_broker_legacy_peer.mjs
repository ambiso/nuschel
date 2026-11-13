// SPDX-License-Identifier: EUPL-1.2
// Bounded, audio-free stock-protocol observer for owned loopback fixtures only.
import tls from 'node:tls';
import assert from 'node:assert/strict';
const MAX=2*1024*1024;
const varint=value=>{const out=[];do{out.push((value&127)|(value>127?128:0));value=Math.floor(value/128);}while(value);return Buffer.from(out);};
const integer=(id,value)=>Buffer.concat([varint(id*8),varint(value)]);
const string=(id,value)=>{const bytes=Buffer.from(value);return Buffer.concat([varint(id*8+2),varint(bytes.length),bytes]);};
function fields(body){let at=0,count=0;const out=new Map();function read(){let value=0n,shift=0n;for(let n=0;n<10;n++){assert.ok(at<body.length);const byte=body[at++];value|=BigInt(byte&127)<<shift;if(!(byte&128))return value<=BigInt(Number.MAX_SAFE_INTEGER)?Number(value):value;shift+=7n;}throw Error('bad varint');}while(at<body.length){assert.ok(++count<=4096);const key=read(),id=Math.floor(key/8),kind=key%8;let value;if(kind===0)value=read();else if(kind===2){const size=read();assert.ok(size<=MAX&&at+size<=body.length);value=body.subarray(at,at+size);at+=size;}else if(kind===1||kind===5){at+=kind===1?8:4;assert.ok(at<=body.length);continue;}else throw Error('wire type');const values=out.get(id)||[];values.push(value);out.set(id,values);}return out;}
export async function legacyPeer(port,username='broker-observer'){
 assert.ok(Number.isInteger(Number(port))&&Number(port)>1023&&Number(port)<=65535);
 const socket=tls.connect({host:'127.0.0.1',port:Number(port),rejectUnauthorized:false});
 const peer={socket,users:new Map(),texts:[],session:null,error:null};let buffered=Buffer.alloc(0),textBytes=0;
 peer.send=(kind,body)=>{assert.ok(body.length<=MAX);const header=Buffer.alloc(6);header.writeUInt16BE(kind);header.writeUInt32BE(body.length,2);socket.write(Buffer.concat([header,body]));};
 peer.message=(channel,body)=>peer.send(11,Buffer.concat([integer(3,channel),string(5,body)]));
 peer.join=channel=>peer.send(9,Buffer.concat([integer(1,peer.session),integer(5,channel),integer(9,1)]));
 peer.wait=async(predicate,reason)=>{const until=Date.now()+20000;while(Date.now()<until){if(peer.error)throw peer.error;if(predicate())return;await new Promise(resolve=>setTimeout(resolve,20));}throw Error(reason);};
 socket.on('error',error=>{peer.error=error;});
 socket.on('data',data=>{try{assert.ok(buffered.length+data.length<=MAX+6);buffered=Buffer.concat([buffered,data]);while(buffered.length>=6){const kind=buffered.readUInt16BE(),size=buffered.readUInt32BE(2);assert.ok(size<=MAX);if(buffered.length<size+6)break;const body=fields(buffered.subarray(6,6+size));buffered=buffered.subarray(6+size);const num=id=>body.get(id)?.[0],text=id=>body.get(id)?.[0]?.toString();if(kind===5)peer.session=num(1);if(kind===9){const session=num(1);assert.ok(peer.users.has(session)||peer.users.size<128);const user={...peer.users.get(session),session};for(const [id,key,decode]of[[3,'name',text],[5,'channel',num],[9,'muted',num],[10,'deafened',num]])if(body.has(id))user[key]=decode(id);peer.users.set(session,user);}if(kind===8)peer.users.delete(num(1));if(kind===11){const value=text(5)||'';textBytes+=Buffer.byteLength(value);assert.ok(peer.texts.length<32&&textBytes<=MAX);peer.texts.push({actor:num(1),body:value,channels:body.get(3)||[]});}}}catch(error){peer.error=error;socket.destroy();}});
 await new Promise((resolve,reject)=>{socket.once('secureConnect',resolve);socket.once('error',reject);});
 peer.send(0,integer(1,0x010500));peer.send(2,Buffer.concat([string(1,username),integer(5,1)]));
 await peer.wait(()=>peer.session!==null,'owned legacy observer authentication');peer.join(1);await peer.wait(()=>peer.users.get(peer.session)?.channel===1,'observer channel join');return peer;
}

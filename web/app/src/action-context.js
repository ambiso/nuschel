// SPDX-License-Identifier: EUPL-1.2
// Original VISIBLE scalar references, never a lookup of newer SDK authority.
const ARITY=Object.freeze({add_server:5,connect:1,disconnect:1,accept_certificate:2,open:2,browse_server:1,open_direct:2,send_text:1,older:0,join:2,toggle_mute:1,toggle_deaf:1,audio_ready:1,voice_packet:1,remove_server:1,dismiss_notice:1,dismiss_welcome:2,connect_with_password:2,stop_audio:0,edit_server:6,set_server_order:1,content_complete:2,content_request:1,image_begin:1,image_send:1,image_cancel:1});
export const UI_METHODS=Object.freeze(Object.keys(ARITY));
const EXEMPT=new Set(["add_server","set_server_order","stop_audio","audio_ready","voice_packet","content_complete","content_request","image_begin","image_send","image_cancel"]);
const LOCAL=new Set(["browse_server","connect","connect_with_password","edit_server","remove_server","dismiss_notice","dismiss_welcome"]);
const TARGET=new Set(["open","join","open_direct"]);
const SELECTED=new Set(["send_text","older"]);
const TOGGLE=new Set(["toggle_mute","toggle_deaf"]);
const stale=()=>{throw new Error("This view changed before the action could run. Your draft was kept; use the updated view and try again.");};
export function projectedAction(method,args){
  if(!Object.hasOwn(ARITY,method)||!Array.isArray(args)||args.length!==ARITY[method])throw new Error("Unsupported browser action arguments.");
  if(EXEMPT.has(method))return null;
  if(SELECTED.has(method))return {method};
  if(TARGET.has(method))return {method,index:args[0],target:args[1]};
  if(method==="accept_certificate")return {method,index:args[0],subject:args[1]};
  return {method,index:args[0]};
}
const counter=value=>typeof value==="string"&&/^(?:0|[1-9][0-9]{0,19})$/.test(value);
export function serverReference(server){
  if(!server||typeof server.id!=="string"||server.id.length>512||!counter(server.catalog_instance)||server.catalog_instance==="0"||!counter(server.generation)||!(server.own_instance===null||counter(server.own_instance))||typeof server.muted!=="boolean"||typeof server.deafened!=="boolean")stale();
  return {id:server.id,catalog_instance:server.catalog_instance,generation:server.generation,own_instance:server.own_instance,muted:server.muted,deafened:server.deafened};
}
function peerReference(peer){
  if(peer==null)return null;
  if(!Number.isInteger(peer.session)||peer.session<0||peer.session>0xffffffff||!counter(peer.instance))stale();
  return {session:peer.session,instance:peer.instance};
}
export function actionProof(method,args,servers,timeline,voice,explicitServer){
  const call=projectedAction(method,args);if(!call)return undefined;
  if(explicitServer&&!LOCAL.has(method))stale();
  const server=explicitServer??(SELECTED.has(method)?servers.find(server=>server.id===timeline?.server):servers.find(server=>server.runtime_index===args[0]));
  const proof={server:serverReference(server)};
  if(SELECTED.has(method)){
    if(!timeline||!counter(timeline.instance)||timeline.server!==server.id)stale();
    proof.selected={channel:timeline.channel,instance:timeline.instance,peer:peerReference(timeline.peer)};
  }else if(TARGET.has(method)){
    const user=method==="open_direct",target=(user?server.users:server.channels)?.find(target=>(user?target.session:target.id)===args[1]);
    if(!target||!counter(target.instance))stale();
    proof.target={kind:user?"user":"channel",id:args[1],instance:target.instance};
    if(!user)proof.target.peer=peerReference(target.peer);
  }else if(TOGGLE.has(method)){
    if(!voice||voice.server_id!==server.id)stale();
    proof.voice={server_id:voice.server_id,connection_generation:voice.connection_generation,voice_generation:voice.voice_generation,channel_id:voice.channel_id,muted:voice.muted,deafened:voice.deafened};
  }
  return proof;
}

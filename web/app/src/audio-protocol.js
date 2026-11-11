// SPDX-License-Identifier: EUPL-1.2
export const RATE=48000, FRAME=960, MAX_PACKET=1024, MAX_PCM=5760, MAX_STREAMS=16, MAX_PENDING=16;
const U64=(1n<<64n)-1n;
export function decimal(value) {
  return typeof value==="string"&&/^(0|[1-9][0-9]{0,19})$/.test(value)&&BigInt(value)<=U64;
}
export function context(value) {
  if(!value||typeof value.server_id!=="string"||value.server_id.length<1||value.server_id.length>128||
      !decimal(value.connection_generation)||!decimal(value.voice_generation)||
      !Number.isInteger(value.channel_id)||value.channel_id<0||value.channel_id>0xffffffff)return null;
  return {server_id:value.server_id,connection_generation:value.connection_generation,
    voice_generation:value.voice_generation,channel_id:value.channel_id};
}
export function sameContext(a,b) {
  return Boolean(a&&b&&a.server_id===b.server_id&&a.connection_generation===b.connection_generation&&a.voice_generation===b.voice_generation);
}
export function sameDestination(a,b) {
  return Boolean(a&&b&&a.server_id===b.server_id&&a.connection_generation===b.connection_generation&&a.channel_id===b.channel_id);
}
export function validPacket(packet,expected) {
  return Boolean(sameContext(packet,expected)&&Number.isInteger(packet.speaker)&&packet.speaker>=0&&packet.speaker<=0xffffffff&&
    decimal(packet.sequence)&&packet.opus instanceof Uint8Array&&packet.opus.length>0&&packet.opus.length<=MAX_PACKET&&
    typeof packet.end==="boolean"&&Number.isFinite(packet.volume)&&packet.volume>=0&&packet.volume<=4);
}

// Isolated browser fixtures for UI regressions. Real candidate assets, no live node,
// no credentials, no network egress, and no services or persistent data changed.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const ORIGIN = 'http://odeion-fixture.test';
const ROOT = path.resolve(process.argv[2] || path.join(__dirname, '..'));
const STATIC = path.join(ROOT, 'crates/pm-api/static');
const SONG = 'https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69';
const LINK = 'https://example.com/liner-notes';
const artists = [{id: 11, name: 'Sefa'}, {id: 22, name: 'Dr. Peacock'}];
const track = {id: 1, title: 'Journey Through Time', artists, album: {id: 7, title: 'Journey'}, duration_ms: 240000, mbid: SONG.split('/').pop(), canonical_uri: SONG, file: {container: 'flac', format: {kind:'pcm',bits:16,rate_hz:44100,channels:2}}};
const summary = {id: 'c1', name: 'Music and notes', items: 2, covers: [], visibility: 'private', synced: false, layout:'list'};
const detail = {summary, playlist:{...summary,items:[{uri:SONG},{uri:LINK}]}, tracks:[track,null], entries:[{kind:'track',uri:SONG},{kind:'link',uri:LINK}], local_tracks:[]};
const pf = {order:{order:['library'],services:[{id:'library',name:'Your library',connected:true}]},own_order:false,items:[{index:0,uri:SONG,title:track.title,artist:'Sefa, Dr. Peacock',track_id:1,plays_from:{service:'library',label:'Your library'},sources:[]},{index:1,uri:LINK,plays_from:null,sources:[]}]};
const json = (r, body, status=200) => r.fulfill({status,contentType:'application/json',body:JSON.stringify(body)});
async function create(browser, width, opts={}) {
 const state = {role:'owner',token:'fixture-owner',offline:false,collections:[],favs:[],hits:[],errors:[],unhandled:[],...opts};
 // opts.mobile: a phone's viewport rules (a page wider than the screen is zoomed out to fit, as Android's WebView does)
 const context = await browser.newContext({viewport:{width,height:900},reducedMotion:'reduce',hasTouch:!!opts.touch||!!opts.mobile,...(opts.mobile?{isMobile:true,deviceScaleFactor:2.625}:{})});
 await context.addInitScript(([realAudio,token,zone]) => {localStorage.setItem('pm.token',token);localStorage.setItem('pm.zone',zone);if(realAudio){document.addEventListener('play',e=>{if(e.target instanceof HTMLMediaElement){e.target.muted=true;e.target.volume=0;}},true);}else{HTMLMediaElement.prototype.play=async function(){};HTMLMediaElement.prototype.pause=function(){};}},[!!state.realAudio,state.token,state.zone||'browser']);
 await context.route('**/*', async r => {
  const req=r.request(), u=new URL(req.url()), p=u.pathname;
  if(u.origin!==ORIGIN) return r.abort();
  if(!p.startsWith('/v1/')) {const f=path.join(STATIC, p==='/'||!path.extname(p)?'index.html':p.slice(1));return fs.existsSync(f)?r.fulfill({body:fs.readFileSync(f),contentType:({'.html':'text/html','.js':'text/javascript','.css':'text/css','.svg':'image/svg+xml'})[path.extname(f)]||'application/octet-stream'}):r.fulfill({status:404,body:''});}
  const v=p.slice(3), m=req.method();state.hits.push({v,m,q:u.search,body:req.postData(),at:Date.now()});
  // opts.delay: ms (or (path, method) => ms) before the answer, for loading states (ui-busy.js)
  const wait=typeof state.delay==='function'?state.delay(v,m):state.delay; if(wait) await new Promise(res=>setTimeout(res,wait));
  if(state.fixtures?.[v]) return json(r,state.fixtures[v].body,state.fixtures[v].status||200);
  if(v==='/info') return json(r,{name:'Fixture node',offline:state.offline,library:{tracks:state.tracks?.length??1,albums:1,artists:2,files:1,missing_files:0}});
  if(v==='/me') return json(r,{role:state.role,key:null,zones:null});
  if(v==='/pubky') return json(r,{user:null});
  if(['/outputs','/zones','/sources','/presence','/smart','/library/albums','/library/artists','/library/genres'].includes(v)) return json(r,[]);
  if(v==='/playlists') {if(m==='POST'){state.collections=[summary];return json(r,detail);}return json(r,state.collections);}
  if(v==='/playlists/c1') return json(r,detail);
  if(v==='/playlists/c1/play-from') return json(r,pf);
  if(v==='/bookmarks'||v==='/favourites') {
   if(m==='PUT') {state.favs=[SONG];return json(r,{uri:SONG});}
   if(m==='DELETE') {state.favs=[];return json(r,{});}
   return json(r,{uris:state.favs,tracks:state.favs.length?[track]:[],albums:[],artists:[],other:[]});
  }
  if(v==='/library/tracks') return json(r,{tracks:state.tracks||[track],next:null});
  if(v==='/library/tracks/1') return json(r,(state.tracks||[track])[0]);
  if(v==='/library/albums/7') return json(r,{...track.album,tracks:state.tracks||[track],artists});
  if(v.startsWith('/item/')) return json(r,{kind:v.split('/')[2],id:v.split('/')[3],title:'Artist page',songs:[],releases:[],links:[],also_on:[],tags:[]});
  if(v==='/node/settings') return json(r,{contact:'',discovery:false,token_replaceable:true});
  if(v==='/me/prefs') return json(r,{});
  if(v==='/play-order') return json(r,{order:['library'],services:[{id:'library',name:'Your library',connected:true}]});
  if(v==='/cards') return json(r,{uri:u.searchParams.get('uri'),status:'found',kind:'Article',name:'Liner notes',site:'example.com',description:'How it was made'});
  if(v==='/storage') return json(r,{homeserver:{mode:'none'}});
  if(v==='/smart/preview') return json(r,{tracks:[],matched:0});
  if(v==='/offline/downloads') return json(r,{tracks:state.tracks||[track],playlists:[{id:'c1',name:summary.name,track_ids:[1],total:1}],favourites:{track_ids:[1]}});
  if(/^\/library\/albums\/\d+\/cover$/.test(v)) return r.fulfill({status:404,body:''}); // no artwork kept
  // pm-offline's GET /v1/offline answer (crates/pm-offline/src/server.rs).
  if(v==='/offline') return json(r,{offline:true,device_id:'fixture-device',synced_at:null,bytes:0,queued:0,tracks:state.tracks||[track],albums:[{...track.album,track_ids:[1],cover:false}],playlists:[{id:'c1',name:summary.name,track_ids:[1],total:1}],favourites:{track_ids:[1]}});
  if(v==='/search') return r.fulfill({contentType:'application/x-ndjson',body:JSON.stringify({source:'library',status:'ok',hits:state.searchHits||[],counts:[]})+'\n'});
  if(v==='/discover/play') return json(r,{uri:SONG,title:track.title,artist:'Sefa, Dr. Peacock',track_id:1});
  state.unhandled.push(v);return json(r,{error:'Fixture endpoint unavailable'},404);
 });
 await context.routeWebSocket(/\/v1\/events/, ws=>{ws.onMessage(m=>{let j;try{j=JSON.parse(m);}catch{return;}if(j.type==='ping')ws.send(JSON.stringify({type:'pong',id:j.id}));});ws.send(JSON.stringify({type:'snapshot',seq:0,boot:'fixture-boot',library_rev:0,zones:[]}));});
 const page=await context.newPage();page.on('pageerror',e=>state.errors.push(e.message));
 page.setDefaultTimeout(7000);
 return {context,page,state};
}
const go = async (page,p) => {await page.goto(ORIGIN+p);};
async function cleanCopy(page) {
 const text = await page.evaluate(() => {
  const attrs=[...document.querySelectorAll('[aria-label],[title],[placeholder]')].filter(e=>e.getClientRects().length).flatMap(e=>['aria-label','title','placeholder'].map(a=>e.getAttribute(a)||''));
  return document.body.innerText+'\n'+attrs.join('\n');
 });
 assert.doesNotMatch(text,/\bundefined\b|\[object Object\]/,'rendered text and accessible labels');
 const overflow=await page.evaluate(()=>document.documentElement.scrollWidth-document.documentElement.clientWidth);assert.ok(overflow<=0,`no horizontal overflow: ${overflow}`);
}
module.exports={create,go,cleanCopy,ORIGIN,track,artists,detail,summary,SONG,LINK};

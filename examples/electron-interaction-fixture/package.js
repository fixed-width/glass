const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
if (process.platform !== 'linux') throw new Error('This fixture packaging script currently supports Linux');
const root = __dirname;
const distribution = path.join(root, 'dist/interaction-fixture-linux');
fs.rmSync(distribution, {recursive: true, force: true});
fs.cpSync(path.join(root, 'node_modules/electron/dist'), distribution, {recursive: true});
fs.renameSync(path.join(distribution, 'electron'), path.join(distribution, 'interaction-fixture'));
fs.rmSync(path.join(distribution, 'resources/default_app.asar'), {force: true});
const application = path.join(distribution, 'resources/app');
fs.mkdirSync(application, {recursive: true});
for (const name of ['main.js', 'preload.js', 'dialog.js']) fs.copyFileSync(path.join(root, name), path.join(application, name));
fs.writeFileSync(path.join(application, 'package.json'), JSON.stringify({name: 'glass-interaction-fixture', version: '1.0.0', main: 'main.js'}));
const shared = path.join(root, '../interaction-fixture');
fs.copyFileSync(path.join(shared, 'fixture.js'), path.join(application, 'fixture.js'));
fs.writeFileSync(path.join(application, 'index.html'), fs.readFileSync(path.join(shared, 'index.html'), 'utf8') + '\n<script src="dialog.js"></script>\n');
const files = {};
for (const name of fs.readdirSync(distribution, {recursive: true}).sort()) {
  const full = path.join(distribution, name);
  if (fs.statSync(full).isFile()) {
    const body = fs.readFileSync(full);
    files[name] = {bytes: body.length, sha256: crypto.createHash('sha256').update(body).digest('hex')};
  }
}
fs.writeFileSync(path.join(distribution, 'fixture-build.json'), JSON.stringify({electron: require('electron/package.json').version, files}, null, 2) + '\n');
console.log(path.join(distribution, 'interaction-fixture'));

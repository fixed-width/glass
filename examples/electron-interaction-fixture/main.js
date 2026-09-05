const {app, BrowserWindow, dialog, ipcMain} = require('electron');
const path = require('node:path');

app.whenReady().then(() => {
  app.setAccessibilitySupportEnabled(true);
  const window = new BrowserWindow({
    width: 1000, height: 700, useContentSize: true,
    webPreferences: {preload: path.join(__dirname, 'preload.js'), contextIsolation: true, sandbox: true}
  });
  window.setMenu(null);
  ipcMain.handle('confirm-account', async (event, value) => {
    if (event.sender !== window.webContents || typeof value !== 'string') throw new Error('Invalid confirmation');
    const result = await dialog.showMessageBox(window, {
      type: 'question', title: 'Confirm account', message: `Confirm saved value: ${value}`,
      buttons: ['Confirm value', 'Cancel'], defaultId: 0, cancelId: 1
    });
    return result.response === 0;
  });
  window.loadFile('index.html', {query: {case: 'large-form'}});
});
app.on('window-all-closed', () => app.quit());

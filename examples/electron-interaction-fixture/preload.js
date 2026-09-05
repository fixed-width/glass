const {contextBridge, ipcRenderer} = require('electron');
contextBridge.exposeInMainWorld('accountDialog', {
  confirm: value => ipcRenderer.invoke('confirm-account', value)
});

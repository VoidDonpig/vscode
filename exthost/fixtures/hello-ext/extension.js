const vscode = require('vscode');
exports.activate = function (context) {
	context.subscriptions.push(vscode.commands.registerCommand('hello.greet', (name) => `Hello ${name} from ${vscode.env.appName}`));
};

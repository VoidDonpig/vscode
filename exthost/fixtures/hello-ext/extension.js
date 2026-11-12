const vscode = require('vscode');
exports.activate = function (context) {
	context.subscriptions.push(
		vscode.commands.registerCommand('hello.greet', (name) => `Hello ${name} from ${vscode.env.appName}`),
		vscode.commands.registerCommand('hello.notify', () => vscode.window.showInformationMessage('Hello from an extension running under Rust')),
		vscode.languages.registerCompletionItemProvider('plaintext', {
			// Echo the current line so the test proves the document text reached the extension.
			provideCompletionItems(doc, pos) {
				return [new vscode.CompletionItem('rust'), new vscode.CompletionItem(`line:${doc.lineAt(pos.line).text}`)];
			}
		}),
	);
};

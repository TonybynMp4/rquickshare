import globals from "globals";
import pluginJs from "@eslint/js";
import tseslint from "typescript-eslint";
import pluginVue from "eslint-plugin-vue";

export default [
	{
		//---- GLOBAL IGNORES
		// note folders can only be ignored at the global level, per-cfg you must do: '**/dist/**/*'
		ignores: [
			'**/dist/',
			'**/vendor/',
			'**/node_modules/',
			'**/target/',
		],
	},
	{files: ["**/*.{js,mjs,cjs,ts,vue}"]},
	{files: ["**/*.js"], languageOptions: {sourceType: "commonjs"}},
	{languageOptions: { globals: globals.browser }},
	// Build and tool configs run in Node, not the webview.
	{files: ["*.config.{js,mjs,cjs,ts}"], languageOptions: { globals: globals.node }},
	pluginJs.configs.recommended,
	...tseslint.configs.recommended,
	...pluginVue.configs["flat/essential"],
	...pluginVue.configs["flat/strongly-recommended"],
	{files: ["**/*.vue"], languageOptions: {parserOptions: {parser: tseslint.parser}}},
	{
		rules: {
			'vue/no-multiple-template-root': "off",
			'no-console': process.env.NODE_ENV === 'production' ? 'warn' : 'off',
			'no-debugger': process.env.NODE_ENV === 'production' ? 'warn' : 'off',
			'vue/max-attributes-per-line': ['error', {
				singleline: {
					max: 4
				},
				multiline: {
					max: 4
				}
			}],
			'vue/html-closing-bracket-newline': ['error', {
				singleline: 'never',
				multiline: 'never'
			}],
			'new-cap': 'off',
			'camelcase': 'off',
			'vue/no-multi-spaces': 'off',
			'vue/multi-word-component-names': 'off',
			'vue/html-indent': ['error', 'tab', {
				attribute: 1,
				baseIndent: 1,
				closeBracket: 0,
				alignAttributesVertically: true,
				ignores: []
			}],
			'vue/script-indent': ['error', 'tab', {
				baseIndent: 0,
				switchCase: 0,
				ignores: []
			}],
			'no-tabs': 0,
			'indent': [2, 'tab'],
			'@typescript-eslint/no-explicit-any': 'off'
		}
	}
];

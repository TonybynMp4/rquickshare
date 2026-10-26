import { test, assert, describe } from 'vitest'
import { displayVersion, isNewerVersion, isReleaseBuild } from '../../src/vue_lib/version'

describe('version', () => {
	test('a stamped release shows four numbers', () => {
		assert.equal(displayVersion('1.26.298+5'), '1.26.298.5')
		assert.equal(displayVersion('1.0.0'), '1.0.0')
	})

	test('only the unstamped <major>.0.0 is a dev build', () => {
		assert.isFalse(isReleaseBuild('1.0.0'))
		assert.isFalse(isReleaseBuild('2.0.0'))
		assert.isTrue(isReleaseBuild('1.26.298.5'))
		assert.isTrue(isReleaseBuild('1.26.1.1'))
		assert.isTrue(isReleaseBuild('1.0.1'))
	})

	test('compares number by number', () => {
		assert.isTrue(isNewerVersion('1.26.299.1', '1.26.298.5'))
		assert.isTrue(isNewerVersion('1.26.298.6', '1.26.298.5'))
		// Numbers, not strings: 10 is after 9.
		assert.isTrue(isNewerVersion('1.26.298.10', '1.26.298.9'))
		assert.isFalse(isNewerVersion('1.26.298.5', '1.26.298.5'))
		assert.isFalse(isNewerVersion('1.26.297.9', '1.26.298.5'))
		assert.isFalse(isNewerVersion('garbage', '1.26.298.5'))
	})
})

"""Offline contract tests: no provider requests or model downloads."""
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch
import embed
import gemini
import jev


class Adapters(unittest.TestCase):
    def test_pretrained_is_required(self):
        factory = Mock()
        factory.create_model_and_transforms.return_value = (Mock(), None, Mock())
        embed.load_model(factory, {"model": "MobileCLIP2-S0", "weights_id": "dfndr2b"})
        factory.create_model_and_transforms.assert_called_once_with(
            "MobileCLIP2-S0", pretrained="dfndr2b", require_pretrained=True)
        factory.create_model_and_transforms.side_effect = RuntimeError("checkpoint unavailable")
        with self.assertRaises(RuntimeError):
            embed.load_model(factory, {"model": "MobileCLIP2-S0", "weights_id": "dfndr2b"})

    def test_gemini_transport_and_sampling(self):
        with tempfile.TemporaryDirectory() as directory:
            video = Path(directory) / "window.mp4"
            video.write_bytes(b"mock video")
            output = Path(directory) / "result.json"
            params = {"mode": "windows", "model": "configured-model", "max_output_tokens": 100,
                      "brief": {"gemini_fps": 12}, "prompt": "analyze", "keeps": [
                      {"id": "f1", "t": 1, "representative_t": 1.1, "file": str(video),
                       "cut_on_beat": True, "onset": .8, "word": "hello"}]}
            response = {"candidates": [{"content": {"parts": [{"text": json.dumps({"beats": []})}]}}],
                        "usageMetadata": {"promptTokenCount": 123, "candidatesTokenCount": 45}}
            transport = Mock(return_value=io.BytesIO(json.dumps(response).encode()))
            with patch('sys.stdin', io.StringIO(json.dumps({"params": params, "out": str(output)}))), \
                 patch.dict('os.environ', {"SCENE_CAP_AUTH": "secret"}), \
                 patch('urllib.request.urlopen', transport):
                gemini.main()
            request = transport.call_args.args[0]
            self.assertIn("configured-model:generateContent", request.full_url)
            self.assertNotIn("secret", request.full_url)
            body = json.loads(request.data)
            parts = body['contents'][0]['parts']
            self.assertEqual(parts[2]['videoMetadata'], {"fps": 12})
            self.assertEqual(json.loads(parts[1]['text'])['word'], 'hello')
            self.assertEqual(json.loads(output.read_text())['usage'], {"input_tokens": 123, "output_tokens": 45})

    def test_jev_separate_contracts(self):
        common = {"brief": {}, "questions": {"choice": ["a", "b"]}}
        route = jev.build_body(dict(common, task="jev_route", state={"candidates": [{"id": "f1"}]}))
        package = jev.build_body(dict(common, task="jev_package", state={"keeps": [{"id": "f1"}]}))
        self.assertIn('f1', route['questions'])
        self.assertEqual(package['questions'], common['questions'])
        self.assertEqual(jev.normalize('jev_package', {"package": "export"}), {"package": "export"})
        for task, result in [('jev_route', {"package": "export"}),
                             ('jev_package', {"answers": {}}),
                             ('jev_package', {"package": "rerun_window"})]:
            with self.assertRaises((ValueError, KeyError)):
                jev.normalize(task, result)

    def test_jev_mock_transport(self):
        for task, state, result in [("jev_route", {"candidates": []}, {"answers": {}}),
                                    ("jev_package", {"keeps": []}, {"package": "export"})]:
            with tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "result.json"
                req = {"out": str(output), "params": {"task": task, "state": state,
                       "brief": {}, "questions": {"choice": []}}}
                transport = Mock(return_value=io.BytesIO(json.dumps(result).encode()))
                with patch('sys.stdin', io.StringIO(json.dumps(req))), \
                     patch.dict('os.environ', {"SCENE_CAP_AUTH": "secret", "JEV_ENDPOINT": "http://mock.invalid/gateway"}), \
                     patch('urllib.request.urlopen', transport):
                    jev.main()
                request = transport.call_args.args[0]
                self.assertEqual(json.loads(request.data)['system'], task)
                self.assertEqual(json.loads(output.read_text()), result)


if __name__ == '__main__':
    unittest.main()

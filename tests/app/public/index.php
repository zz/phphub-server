<?php
// Test worker: one object built before the loop, request data checked per request.
final class App {
    public int $served = 0;
    public array $leak = [];
}
$app = new App();
$bootedAt = microtime(true);
echo "bootstrap output must not reach any client\n";

while (phphub_handle_request()) {
    $app->served++;
    $path = parse_url($_SERVER['REQUEST_URI'], PHP_URL_PATH);
    switch ($path) {
        case '/warn':
            trigger_error('test warning from worker', E_USER_WARNING);
            echo 'warned';
            break;
        case '/sleep':
            usleep((int)($_GET['ms'] ?? 0) * 1000);
            echo 'slept';
            break;
        case '/upload-spin':
            file_put_contents('/tmp/last-upload', $_FILES['f']['tmp_name'] ?? 'none');
            while (true) { }
        case '/stream-spin':
            echo "part1\n"; flush();
            while (true) { }
        case '/after':
            // Answer, then keep the thread busy outside any request.
            echo 'answered';
            phphub_finish_request();
            usleep((int)($_GET['ms'] ?? 0) * 1000);
            break;
        case '/orphan':
            // The shell exits at once; its background sleep becomes PID 1's.
            exec('sh -c "sleep 0.2 >/dev/null 2>&1 &"');
            echo 'orphaned';
            break;
        case '/exitcodes':
            // PHP waits for these children itself; their codes must survive.
            exec('sh -c "sleep 0.1; exit 7"', $out, $rc1);
            system('sh -c "exit 3"', $rc2);
            $p = proc_open(['sh', '-c', 'sleep 0.1; exit 5'], [], $pipes);
            $rc3 = proc_close($p);
            echo "$rc1 $rc2 $rc3\n";
            break;
        case '/spin':
            while (true) { }
        case '/fatal':
            undefined_function_call();
            break;
        case '/header':
            header('X-Set-Once: yes');
            setcookie('c1', 'v1');
            echo 'header';
            break;
        case '/cgi-status':
            if (isset($_GET['code'])) http_response_code((int)$_GET['code']);
            if (isset($_GET['loc'])) header('Location: ' . $_GET['loc']);
            header(($_GET['name'] ?? 'Status') . ': ' . $_GET['s']);
            if (isset($_GET['flush'])) { echo "part1\n"; flush(); }
            echo 'cgi';
            break;
        case '/status':
            http_response_code(201);
            echo 'created';
            break;
        case '/big':
            echo str_repeat('x', (int)($_GET['n'] ?? 3000000));
            break;
        case '/stream':
            echo "part1\n"; flush(); usleep(200000); echo "part2\n";
            break;
        case '/upload':
            $f = $_FILES['f'] ?? null;
            echo json_encode(['name' => $f['name'] ?? null, 'size' => $f['size'] ?? null,
                'tmp_exists' => $f ? file_exists($f['tmp_name']) : null, 'tmp' => $f['tmp_name'] ?? null,
                'post' => $_POST]);
            break;
        default:
            header('Content-Type: application/json');
            echo json_encode([
                'app_id' => spl_object_id($app),
                'served' => $app->served,
                'booted_at' => $bootedAt,
                'get' => $_GET,
                'post' => $_POST,
                'cookie' => $_COOKIE,
                'method' => $_SERVER['REQUEST_METHOD'],
                'uri' => $_SERVER['REQUEST_URI'],
                'script_name' => $_SERVER['SCRIPT_NAME'],
                'script_filename' => $_SERVER['SCRIPT_FILENAME'],
                'php_self' => $_SERVER['PHP_SELF'],
                'path_info' => $_SERVER['PATH_INFO'] ?? null,
                'remote_addr' => $_SERVER['REMOTE_ADDR'],
                'xff' => $_SERVER['HTTP_X_FORWARDED_FOR'] ?? null,
                'xfp' => $_SERVER['HTTP_X_FORWARDED_PROTO'] ?? null,
                'xri' => $_SERVER['HTTP_X_REAL_IP'] ?? null,
                'body' => file_get_contents('php://input'),
                'sapi' => PHP_SAPI,
                'phalcon' => extension_loaded('phalcon'),
                'opcache' => function_exists('opcache_get_status') && (opcache_get_status(false)['opcache_enabled'] ?? false),
                'zts' => PHP_ZTS,
                'thread_id' => getmypid(),
            ]);
    }
    phphub_finish_request();
}

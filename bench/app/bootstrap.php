<?php
// The same Phalcon application for both servers. Built once per thread.
namespace Bench\Controllers {
    use Phalcon\Http\Response;
    use Phalcon\Mvc\Controller;

    class IndexController extends Controller
    {
        public function indexAction(): Response
        {
            $e = $this->di->getShared('escaper');
            $rows = '';
            for ($i = 1; $i <= 20; $i++) {
                $rows .= '<li><a href="/item/' . $i . '">' . $e->html("Item #$i <b>") . '</a></li>';
            }
            $html = '<!doctype html><html><head><title>Bench</title></head><body><ul>' . $rows . '</ul></body></html>';
            $r = new Response();
            $r->setContentType('text/html', 'UTF-8');
            $r->setContent($html);
            return $r;
        }

        public function helloAction(): Response
        {
            $r = new Response();
            $r->setContent('hello');
            return $r;
        }

        /** About 120 KB of HTML, the size of a real listing page: sizes the response copy. */
        public function bigAction(): Response
        {
            static $html = null;
            $html ??= '<!doctype html><html><body>' . str_repeat('<p>' . str_repeat('lorem ipsum ', 20) . '</p>', 500) . '</body></html>';
            $r = new Response();
            $r->setContentType('text/html', 'UTF-8');
            $r->setContent($html);
            return $r;
        }
    }

    class ItemsController extends Controller
    {
        public function showAction(int $id): Response
        {
            $r = new Response();
            $r->setJsonContent([
                'id' => $id,
                'name' => 'Item ' . $id,
                'tags' => array_map(fn ($t) => "tag$t", range(1, 5)),
                'query' => $this->request->getQuery('q', 'string', ''),
                'app' => spl_object_id($this->di->getShared('application')),
            ]);
            return $r;
        }
    }
}

namespace {
    use Phalcon\Di\FactoryDefault;
    use Phalcon\Mvc\Application;
    use Phalcon\Mvc\Dispatcher;
    use Phalcon\Mvc\Router;

    $di = new FactoryDefault();
    $di->setShared('router', function () {
        $router = new Router(false);
        $router->removeExtraSlashes(true);
        $router->add('/', ['controller' => 'index', 'action' => 'index']);
        $router->add('/hello', ['controller' => 'index', 'action' => 'hello']);
        $router->add('/big', ['controller' => 'index', 'action' => 'big']);
        $router->add('/item/{id:[0-9]+}', ['controller' => 'items', 'action' => 'show']);
        $router->notFound(['controller' => 'index', 'action' => 'hello']);
        return $router;
    });
    $di->setShared('dispatcher', function () {
        $d = new Dispatcher();
        $d->setDefaultNamespace('Bench\Controllers');
        return $d;
    });

    $app = new Application($di);
    $app->useImplicitView(false);
    $di->setShared('application', $app);
    return $app;
}
